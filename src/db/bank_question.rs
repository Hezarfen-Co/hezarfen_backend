//! The `bank_question` table: template creates, the visibility-gated paged
//! listing, the page-wide usage tally, the compare-and-set every PATCH writes
//! through, and the delete that sweeps the slot table and clears both
//! provenance links in one transaction. The PATCH re-derive lives in
//! [`crate::service::bank_question`]; the pure entity and newtypes in
//! [`crate::domain::bank_question`].

use std::collections::HashMap;

use sqlx::types::Json;

use crate::database::{Database, tx_with_retry};
use crate::db::page::{PagedList, Param};
use crate::domain::bank_question::{BankQuestion, BankQuestionId, BankVisibility};
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    Choice, ChoiceId, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::note_file::FileContentType;
use crate::domain::subject::SubjectId;
use crate::domain::text_fold::{search_fold, search_fold_sql};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The `choices` JSONB bind as the macros type the parameter — a
/// `serde_json::Value`. A `Choice` is two plain strings: serializing one
/// cannot fail.
fn choices_as_value(choices: &[Choice]) -> serde_json::Value {
    serde_json::to_value(choices).expect("Choice serialization cannot fail")
}

pub async fn create(
    db: &Database,
    owner: UserId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<BankQuestion, AppError> {
    insert(db, owner, subject, text, points, spec, None).await
}

/// Like [`create`], but records the origin exam the template was saved from
/// (`POST …/questions/{qid}/to-bank`).
pub async fn create_from_exam(
    db: &Database,
    owner: UserId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    source: ExamId,
) -> Result<BankQuestion, AppError> {
    insert(db, owner, subject, text, points, spec, Some(source)).await
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    db: &Database,
    owner: UserId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    source_exam: Option<ExamId>,
) -> Result<BankQuestion, AppError> {
    let question = BankQuestion::from_parts(
        BankQuestionId::generate(),
        owner,
        // Creation always carries a validated subject; only a subject
        // delete ever clears it.
        Some(subject),
        text,
        points,
        spec,
        source_exam,
        // Never published on creation — publishing is its own PATCH.
        BankVisibility::default(),
        Timestamp::now(),
    );
    let row = sqlx::query_as!(
        BankQuestion,
        r#"INSERT INTO bank_question
               (id, owner, subject, text, kind, points, choices, correct,
                source_exam, visibility, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           RETURNING id AS "id: BankQuestionId", owner AS "owner: UserId",
               subject AS "subject: SubjectId", text AS "text: QuestionText",
               kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
               choices AS "choices: Json<Vec<Choice>>",
               correct AS "correct: ChoiceId",
               source_exam AS "source_exam: ExamId",
               visibility AS "visibility: BankVisibility",
               created_at AS "created_at: Timestamp""#,
        question.id.uuid(),
        question.owner.uuid(),
        question.subject.as_ref().map(SubjectId::uuid),
        question.text.as_str(),
        question.kind.as_str(),
        question.points.as_i64(),
        question
            .choices
            .as_ref()
            .map(|json| choices_as_value(&json.0)),
        question.correct.as_ref().map(ChoiceId::as_str),
        question.source_exam.as_ref().map(ExamId::uuid),
        question.visibility.as_str(),
        question.created_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(row)
}

pub async fn read(db: &Database, id: &BankQuestionId) -> Result<Option<BankQuestion>, AppError> {
    let row = sqlx::query_as!(
        BankQuestion,
        r#"SELECT id AS "id: BankQuestionId", owner AS "owner: UserId",
                  subject AS "subject: SubjectId", text AS "text: QuestionText",
                  kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
                  choices AS "choices: Json<Vec<Choice>>",
                  correct AS "correct: ChoiceId",
                  source_exam AS "source_exam: ExamId",
                  visibility AS "visibility: BankVisibility",
                  created_at AS "created_at: Timestamp"
           FROM bank_question WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// One page of the bank the caller may see, **newest first** (`id DESC` is
/// mint order — the ids are write-ordered UUIDv7) — plus the total row count
/// under the same filters, so a client can page past the window.
///
/// The window is a real `LIMIT`/`OFFSET` handed to the database (the
/// [`PagedList`] builder: the filters are runtime-conditional, which is that
/// builder's named exemption from the compile-time macros). `q` is a case-
/// and diacritic-insensitive fragment of the question text (blank = no text
/// filter): needle and column both go through [`crate::domain::text_fold`],
/// so `istanbul` finds `İSTANBUL` and back. `position` rather than `LIKE`
/// keeps the needle a *literal* substring — no `%`/`_` wildcard surprises.
///
/// `visible_to` is the security filter, and it is a WHERE clause like every
/// other one — never a post-filter, or `total` would count templates the
/// caller can't have and paging would return short pages of them:
/// `Some(me)` sees the school-published templates plus their own,
/// `None` (admins only) sees everything.
///
/// `visibility` is the *user's* filter ("only mine" / "shared with the
/// school") and is ANDed on top of that gate, so it can only ever narrow:
/// `school` still hides another teacher's private rows, and `private`
/// intersected with the gate leaves exactly the caller's own drafts. It is
/// part of the same WHERE, so `total` honours it too — a count that ignored
/// it would break paging.
///
/// The uuid filters bind raw through the id newtypes' [`crate::domain`]
/// `uuid()` accessor — [`Param::Uuid`] keeps the comparison exact, typed
/// rather than textual.
#[allow(clippy::too_many_arguments)]
pub async fn list(
    db: &Database,
    visible_to: Option<&UserId>,
    owner: Option<&UserId>,
    subject: Option<&SubjectId>,
    visibility: Option<&BankVisibility>,
    q: Option<&str>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<BankQuestion>, i64), AppError> {
    let needle = q.map(|q| search_fold(q.trim())).filter(|q| !q.is_empty());
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<Param> = Vec::new();
    if let Some(viewer) = visible_to {
        binds.push(Param::Uuid(viewer.uuid()));
        clauses.push(format!(
            "(visibility = 'school' OR owner = ${})",
            binds.len()
        ));
    }
    if let Some(visibility) = visibility {
        binds.push(Param::Text(visibility.as_str().to_string()));
        clauses.push(format!("visibility = ${}", binds.len()));
    }
    if let Some(owner) = owner {
        binds.push(Param::Uuid(owner.uuid()));
        clauses.push(format!("owner = ${}", binds.len()));
    }
    if let Some(subject) = subject {
        binds.push(Param::Uuid(subject.uuid()));
        clauses.push(format!("subject = ${}", binds.len()));
    }
    if let Some(needle) = needle {
        binds.push(Param::Text(needle));
        clauses.push(format!(
            "position(${} in {}) > 0",
            binds.len(),
            search_fold_sql("text")
        ));
    }
    let from_where = if clauses.is_empty() {
        "bank_question WHERE true".to_string()
    } else {
        format!("bank_question WHERE {}", clauses.join(" AND "))
    };
    // The count runs over the *same* WHERE, so `total` can never disagree
    // with what paging through the list actually yields.
    let mut page = PagedList::new(from_where, "ORDER BY id DESC");
    for bind in binds {
        page = page.bind(bind);
    }
    page.run(limit, offset, db).await
}

/// How many exam questions were instantiated from each of `ids` — the
/// `from_bank` side of the provenance link, tallied for a whole page in
/// **one** grouped query (a `count()` per row would be the N+1 this page
/// already had removed once). Keys are the templates' wire keys; a template
/// nobody ever used has no entry at all, so the caller reads a miss as
/// zero — and the UI can stay quiet rather than print "0".
///
/// Counts *questions*, not distinct exams: one exam that inserted the same
/// template twice counts twice, which is what "copies made from this
/// template" means and what the divergence trap is actually about.
pub async fn usage_counts(
    db: &Database,
    ids: &[&BankQuestionId],
) -> Result<HashMap<String, i64>, AppError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<uuid::Uuid> = ids.iter().map(|id| id.uuid()).collect();
    let rows = sqlx::query!(
        r#"SELECT from_bank AS "from_bank: BankQuestionId", count(*) AS n
           FROM exam_question
           WHERE from_bank = ANY($1)
           GROUP BY from_bank"#,
        &ids,
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let n = row.n.unwrap_or(0);
            row.from_bank.map(|from_bank| (from_bank.key(), n))
        })
        .collect())
}

/// Write the editable fields of the template, but only while the row still
/// reads as the snapshot `expected` was loaded from — `None` means a concurrent
/// edit (or the subject-delete cascade that clears `subject`) landed inside
/// the handler's read-merge window, so nothing was written: reload,
/// re-merge, retry. This is a genuine read-modify-write — every editable
/// field is re-stated from the snapshot, and the kind/choices/correct trio
/// has to be (a text-only edit re-submits the stored options *with their
/// ids* so each keeps its picture) — so without the compare-and-set the
/// later writer silently reverts the earlier one.
///
/// Every field the merge re-states is in the guard, which is the same list
/// the `SET` writes; the nullable ones compare `IS NOT DISTINCT FROM`, so an
/// absent subject/correct/choices guards truthfully. Postgres needs no retry
/// loop around this: the guard re-evaluates on the row lock at execution, so
/// a rival write either predates this statement's snapshot or loses its own.
pub async fn update_if_unchanged(
    db: &Database,
    expected: BankQuestion,
    subject: Option<SubjectId>,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    visibility: BankVisibility,
) -> Result<Option<BankQuestion>, AppError> {
    let (kind, choices, correct) = spec.into_parts();
    let choices = choices.map(Json);
    let row = sqlx::query_as!(
        BankQuestion,
        r#"UPDATE bank_question SET
               subject = $2, text = $3, points = $4, kind = $5,
               choices = $6, correct = $7, visibility = $8
           WHERE id = $1
             AND subject IS NOT DISTINCT FROM $9
             AND text = $10
             AND points = $11
             AND kind = $12
             AND choices IS NOT DISTINCT FROM $13
             AND correct IS NOT DISTINCT FROM $14
             AND visibility = $15
           RETURNING id AS "id: BankQuestionId", owner AS "owner: UserId",
               subject AS "subject: SubjectId", text AS "text: QuestionText",
               kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
               choices AS "choices: Json<Vec<Choice>>",
               correct AS "correct: ChoiceId",
               source_exam AS "source_exam: ExamId",
               visibility AS "visibility: BankVisibility",
               created_at AS "created_at: Timestamp""#,
        expected.id.uuid(),
        subject.as_ref().map(SubjectId::uuid),
        text.as_str(),
        points.as_i64(),
        kind.as_str(),
        choices.as_ref().map(|json| choices_as_value(&json.0)),
        correct.as_ref().map(ChoiceId::as_str),
        visibility.as_str(),
        expected.subject.as_ref().map(SubjectId::uuid),
        expected.text.as_str(),
        expected.points.as_i64(),
        expected.kind.as_str(),
        expected
            .choices
            .as_ref()
            .map(|json| choices_as_value(&json.0)),
        expected.correct.as_ref().map(ChoiceId::as_str),
        expected.visibility.as_str(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Delete the template and cascade-remove its bank images, so none points
/// at a missing template. Bank rows have no answers. The image rows this
/// actually removed come back with it: the blobs are the web layer's to
/// take off disk, but only for *these* rows — an upload that committed
/// after the caller listed the template's images is swept here too, and a
/// pre-read snapshot would strand its blob for good.
///
/// All four statements are one transaction, children before the parent so
/// the foreign keys never refuse the parent delete: the provenance links
/// clear first (exam questions tied to this template keep living — only the
/// links go, field-scoped, in both directions: `from_bank` on the questions
/// instantiated from it and `banked_as` on the question it was saved out
/// of), then the image sweep, then the row. `cascade = true` because an
/// image upsert committing inside this window makes the parent delete
/// answer 23503 — a mid-cascade race the retry loop re-runs, sweeping the
/// latecomer with it.
pub async fn delete(
    db: &Database,
    target: BankQuestion,
) -> Result<(BankQuestion, Vec<BankQuestionImage>), AppError> {
    tx_with_retry(db, true, async move |tx| {
        let id = target.id.clone();
        sqlx::query!(
            "UPDATE exam_question SET from_bank = NULL WHERE from_bank = $1",
            id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE exam_question SET banked_as = NULL WHERE banked_as = $1",
            id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        let images = sqlx::query_as!(
            BankQuestionImage,
            r#"DELETE FROM bank_question_image WHERE bank_question = $1
               RETURNING bank_question AS "bank_question: BankQuestionId",
                     slot AS "slot: ChoiceId", file,
                     content_type AS "content_type: FileContentType", size"#,
            id.uuid()
        )
        .fetch_all(&mut *tx)
        .await?;
        let question = sqlx::query_as!(
            BankQuestion,
            r#"DELETE FROM bank_question WHERE id = $1
               RETURNING id AS "id: BankQuestionId", owner AS "owner: UserId",
                     subject AS "subject: SubjectId",
                     text AS "text: QuestionText", kind AS "kind: QuestionKind",
                     points AS "points: QuestionPoints",
                     choices AS "choices: Json<Vec<Choice>>",
                     correct AS "correct: ChoiceId",
                     source_exam AS "source_exam: ExamId",
                     visibility AS "visibility: BankVisibility",
                     created_at AS "created_at: Timestamp""#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        Ok((question, images))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{BANK_VISIBILITY_PRIVATE, BANK_VISIBILITY_SCHOOL};

    /// A real `app_user` row: template owners are foreign keys now. The id is
    /// minted per call, so repeated calls are new people, not the same row.
    async fn a_person(db: &Database, label: &str) -> UserId {
        let user = UserId::generate();
        sqlx::query("INSERT INTO app_user (id, username, password_hash) VALUES ($1, $2, 'x')")
            .bind(user.uuid())
            .bind(format!("{label}-{}", &user.key()[30..]))
            .execute(db)
            .await
            .unwrap();
        user
    }

    /// A real subject row: a template's subject reference is a foreign key
    /// too (and the subject needs a real course under it).
    async fn a_subject(db: &Database) -> SubjectId {
        *crate::db::subject::create(
            db,
            &crate::db::course::a_test_course(db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap()
        .get_id()
    }

    fn spec() -> QuestionSpec {
        use crate::domain::exam_question::{ChoiceInput, QuestionKind};
        QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(vec![
                ChoiceInput {
                    id: Some("a".into()),
                    text: "yes".into(),
                },
                ChoiceInput {
                    id: Some("b".into()),
                    text: "no".into(),
                },
            ]),
            Some("b".into()),
            &[],
        )
        .unwrap()
    }

    /// A row without a visibility of its own must decode as `private`, the
    /// safe value. The old engine could store a row with the field absent;
    /// Postgres carries the same guarantee in the schema instead — the column
    /// is `NOT NULL DEFAULT 'private'`, so a `school` default here would
    /// publish every pre-existing template in the school at once. The insert
    /// below is written exactly as a pre-visibility row arrives: no column.
    #[tokio::test]
    async fn a_row_without_the_field_decodes_private() {
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = a_person(&db, "owner").await;
        let id = BankQuestionId::generate();
        sqlx::query(
            "INSERT INTO bank_question (id, owner, text, kind, points, created_at) \
             VALUES ($1, $2, 'q', 'multiple_choice', 1, 0)",
        )
        .bind(id.uuid())
        .bind(owner.uuid())
        .execute(&db)
        .await
        .unwrap();

        let old = read(&db, &id).await.unwrap().expect("the row is there");
        assert_eq!(old.get_visibility().as_str(), BANK_VISIBILITY_PRIVATE);
        assert!(!old.get_visibility().is_school());
    }

    /// The visibility gate is a WHERE clause, so `total` counts exactly what the
    /// page can contain — never someone else's private templates.
    #[tokio::test]
    async fn list_hides_private_templates_from_others() {
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = a_person(&db, "owner").await;
        let other = a_person(&db, "other").await;
        let private = create(
            &db,
            owner,
            a_subject(&db).await,
            QuestionText::try_new("secret").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
        )
        .await
        .unwrap();
        let published = create(
            &db,
            owner,
            a_subject(&db).await,
            QuestionText::try_new("shared").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
        )
        .await
        .unwrap();
        let subject = published.get_subject().cloned();
        update_if_unchanged(
            &db,
            published,
            subject,
            QuestionText::try_new("shared").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap(),
        )
        .await
        .unwrap();

        // The stranger sees the published one alone, and `total` agrees.
        let (items, total) = list(&db, Some(&other), None, None, None, None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(items[0].get_text().as_str(), "shared");
        // The owner sees both; an admin (`None`) too.
        let (items, total) = list(&db, Some(&owner), None, None, None, None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (2, 2));
        let (items, total) = list(&db, None, None, None, None, None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (2, 2));
        // An explicit `owner=` filter can't widen the gate.
        let (items, total) = list(&db, Some(&other), Some(&owner), None, None, None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(private.get_visibility().as_str(), BANK_VISIBILITY_PRIVATE);
    }

    /// The bite test for the compare-and-set that replaced the writer lease of
    /// `BANK_LOCK` on the PATCH path: a merge built on a snapshot the row has
    /// since moved past must be refused, not written — otherwise it reverts
    /// whatever landed in between. Asserts the *stored* row, never a race
    /// outcome: the in-memory engine can drop one of two concurrent writes and
    /// still answer `Ok`.
    #[tokio::test]
    async fn a_merge_built_on_a_stale_snapshot_is_refused() {
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = a_person(&db, "owner").await;
        let stale = create(
            &db,
            owner,
            a_subject(&db).await,
            QuestionText::try_new("first").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
        )
        .await
        .unwrap();
        // Somebody else's edit lands on the row this snapshot came from.
        let landed = update_if_unchanged(
            &db,
            stale.clone(),
            None,
            QuestionText::try_new("theirs").unwrap(),
            QuestionPoints::try_new(2).unwrap(),
            spec(),
            BankVisibility::try_new(BANK_VISIBILITY_PRIVATE).unwrap(),
        )
        .await
        .unwrap();
        assert!(landed.is_some(), "the first write is on a fresh snapshot");

        let refused = update_if_unchanged(
            &db,
            stale,
            None,
            QuestionText::try_new("mine").unwrap(),
            QuestionPoints::try_new(3).unwrap(),
            spec(),
            BankVisibility::try_new(BANK_VISIBILITY_PRIVATE).unwrap(),
        )
        .await
        .unwrap();
        assert!(refused.is_none(), "a stale merge must not be written");
        let stored = read(&db, landed.unwrap().get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_text().as_str(), "theirs");
        assert_eq!(stored.get_points().as_i64(), 2);
    }

    /// The `visibility` filter ANDs onto the security gate, so it narrows and
    /// never widens: a stranger asking for `school` still can't see a private
    /// row, and `private` means "my own drafts" for everyone but an admin.
    #[tokio::test]
    async fn visibility_filter_narrows_never_widens() {
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = a_person(&db, "owner").await;
        let other = a_person(&db, "other").await;
        async fn mine(
            db: &Database,
            owner: &UserId,
            text: &str,
        ) -> Result<BankQuestion, AppError> {
            create(
                db,
                *owner,
                a_subject(db).await,
                QuestionText::try_new(text).unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
            )
            .await
        }
        mine(&db, &owner, "draft").await.unwrap();
        let published = mine(&db, &owner, "shared").await.unwrap();
        let subject = published.get_subject().cloned();
        update_if_unchanged(
            &db,
            published,
            subject,
            QuestionText::try_new("shared").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap(),
        )
        .await
        .unwrap();
        let private = BankVisibility::try_new(BANK_VISIBILITY_PRIVATE).unwrap();
        let school = BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap();

        // Owner: `private` = their drafts, `school` = the published one.
        let (items, total) = list(&db, Some(&owner), None, None, Some(&private), None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(items[0].get_text().as_str(), "draft");
        let (items, total) = list(&db, Some(&owner), None, None, Some(&school), None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(items[0].get_text().as_str(), "shared");
        // A stranger's `private` is empty — never someone else's draft.
        let (items, total) = list(&db, Some(&other), None, None, Some(&private), None, None, 0)
            .await
            .unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);
        // …and their `school` stops at the published one, gate intact.
        let (items, total) = list(&db, Some(&other), None, None, Some(&school), None, None, 0)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        // Composes with the other filters: right owner, wrong visibility.
        let (items, total) = list(
            &db,
            Some(&owner),
            Some(&owner),
            None,
            Some(&school),
            Some("draft"),
            None,
            0,
        )
        .await
        .unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);
    }

    /// The whole page's tally in one grouped statement: right number per
    /// template, unused templates absent (so the UI prints nothing rather than
    /// "0"), and questions authored by hand never counted.
    #[tokio::test]
    async fn usage_counts_tallies_a_page_in_one_statement() {
        // One statement, so a page costs one round trip: the tally is a single
        // static `query!` (a `GROUP BY` with no loop around it), which the
        // compiler now checks the way the old constant asserted.
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = a_person(&db, "owner").await;
        async fn template(
            db: &Database,
            owner: &UserId,
            text: &str,
        ) -> Result<BankQuestion, AppError> {
            create(
                db,
                *owner,
                a_subject(db).await,
                QuestionText::try_new(text).unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
            )
            .await
        }
        let used_twice = template(&db, &owner, "twice").await.unwrap();
        let used_once = template(&db, &owner, "once").await.unwrap();
        let unused = template(&db, &owner, "never").await.unwrap();

        // Real exam rows: instantiating a template moves the exam's counter
        // (what keeps a question from outliving its exam), so a minted id
        // nothing wrote is a 404.
        let exam = crate::db::exam::published_exam(&db).await.get_id().clone();
        let other_exam = crate::db::exam::published_exam(&db).await.get_id().clone();
        // A real subject row: an exam question claims a reference on its
        // subject, so a minted id it never wrote would be refused.
        let subject = crate::db::subject::create(
            &db,
            &crate::db::course::a_test_course(&db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        async fn instantiate(
            exam: &ExamId,
            subject: &SubjectId,
            source: &BankQuestion,
            db: &Database,
        ) {
            crate::db::exam_question::create_from_bank(
                db,
                exam,
                *subject,
                source.get_text().clone(),
                source.get_points(),
                source.spec(),
                source.get_id().clone(),
            )
            .await
            .unwrap();
        }
        // Two copies of the same template, in two different exams.
        instantiate(&exam, subject.get_id(), &used_twice, &db).await;
        instantiate(&other_exam, subject.get_id(), &used_twice, &db).await;
        instantiate(&exam, subject.get_id(), &used_once, &db).await;
        // A hand-authored question has no `from_bank` and must not be tallied.
        crate::db::exam_question::create(
            &db,
            &exam,
            *subject.get_id(),
            QuestionText::try_new("mine").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
        )
        .await
        .unwrap();

        let ids = [used_twice.get_id(), used_once.get_id(), unused.get_id()];
        let counts = usage_counts(&db, &ids).await.unwrap();
        assert_eq!(counts.get(&used_twice.get_id().key()).copied(), Some(2));
        assert_eq!(counts.get(&used_once.get_id().key()).copied(), Some(1));
        assert_eq!(
            counts.get(&unused.get_id().key()),
            None,
            "unused stays absent"
        );
        assert_eq!(counts.len(), 2);

        // A template outside the page is never counted into it.
        let narrow = usage_counts(&db, &[used_once.get_id()]).await.unwrap();
        assert_eq!(narrow.len(), 1);
        assert_eq!(narrow.get(&used_once.get_id().key()).copied(), Some(1));
        // An empty page asks nothing at all.
        assert!(usage_counts(&db, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_is_school_wide() {
        let (db, _leases) = crate::database::init_test_db().await;
        for _ in 0..2 {
            create(
                &db,
                a_person(&db, "owner").await,
                a_subject(&db).await,
                QuestionText::try_new("q").unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
            )
            .await
            .unwrap();
        }
        // Different owners, yet both listed.
        let (items, total) = list(&db, None, None, None, None, None, None, 0)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(total, 2);
    }

    /// The window is SQL, not an in-memory slice, and `total` ignores it.
    #[tokio::test]
    async fn list_pages_newest_first_and_filters_text() {
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = a_person(&db, "owner").await;
        for i in 0..5 {
            create(
                &db,
                owner,
                a_subject(&db).await,
                QuestionText::try_new(&format!("question {i}")).unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
            )
            .await
            .unwrap();
        }
        let (page, total) = list(&db, None, None, None, None, None, Some(2), 0)
            .await
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(page.len(), 2);
        // Newest first: the last-created row leads.
        assert_eq!(page[0].get_text().as_str(), "question 4");
        // The tail is reachable by offset.
        let (tail, _) = list(&db, None, None, None, None, None, Some(2), 4)
            .await
            .unwrap();
        assert_eq!(tail[0].get_text().as_str(), "question 0");
        // Text filter runs in SQL, case-insensitively, and narrows `total`.
        let (hits, total) = list(&db, None, None, None, None, Some("QUESTION 3"), None, 0)
            .await
            .unwrap();
        assert_eq!((hits.len(), total), (1, 1));
        // Owner filter with nobody's templates.
        let (none, total) = list(
            &db,
            None,
            Some(&a_person(&db, "empty").await),
            None,
            None,
            None,
            None,
            0,
        )
        .await
        .unwrap();
        assert!(none.is_empty());
        assert_eq!(total, 0);
    }

    /// Turkish `İ`/`ı` must not split the search into two disjoint halves.
    #[tokio::test]
    async fn list_text_filter_folds_turkish_casing() {
        let (db, _leases) = crate::database::init_test_db().await;
        create(
            &db,
            a_person(&db, "owner").await,
            a_subject(&db).await,
            QuestionText::try_new("İSTANBUL kaç ilçeye ayrılır?").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
        )
        .await
        .unwrap();
        for needle in [
            "istanbul",
            "İSTANBUL",
            "İstanbul",
            "ıstanbul",
            "ilçe",
            "ILCE",
        ] {
            let (hits, total) = list(&db, None, None, None, None, Some(needle), None, 0)
                .await
                .unwrap();
            assert_eq!((hits.len(), total), (1, 1), "needle {needle} missed");
        }
        let (miss, total) = list(&db, None, None, None, None, Some("ankara"), None, 0)
            .await
            .unwrap();
        assert!(miss.is_empty());
        assert_eq!(total, 0);
    }
}
