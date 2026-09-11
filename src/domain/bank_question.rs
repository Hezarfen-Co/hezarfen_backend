//! A reusable question template in the school-wide question bank. Unlike an
//! [`crate::domain::exam_question::ExamQuestion`], a bank row is fully detached
//! from any exam: no exam FK, so no attempt ever freezes it, and its images
//! live in their own [`crate::domain::bank_question_image`] slot table. Teachers
//! save templates here and later *copy* them into an exam — the copy is a fresh
//! `ExamQuestion` with its own id, images, and answers; the two never share a
//! row. The kind-dependent columns satisfy the [`QuestionSpec`] invariants
//! because every write goes through one.

use std::collections::HashMap;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    BANK_QUESTION_TABLE, BANK_VISIBILITY_PRIVATE, BANK_VISIBILITY_SCHOOL, USAGE_COUNTS_SQL,
};
use crate::database::{Database, transaction_with_retry, write_with_retry};
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    Choice, ChoiceId, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::subject::SubjectId;
use crate::domain::text_fold::{search_fold, search_fold_sql};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Who may see a template. `private` — the owner (and admins) alone; `school` —
/// every teacher.
///
/// **The default is `private`, deliberately.** A template carries `correct`, the
/// answer key, and its images: publishing one is an explicit act, never a side
/// effect of saving a question to the bank. `#[surreal(default)]` on the field
/// means rows written before this existed decode as `private` too, so the bank
/// can't retroactively broadcast anyone's answer keys.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BankVisibility(String);

impl Default for BankVisibility {
    fn default() -> Self {
        Self(BANK_VISIBILITY_PRIVATE.to_string())
    }
}

impl BankVisibility {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        if value != BANK_VISIBILITY_PRIVATE && value != BANK_VISIBILITY_SCHOOL {
            return Err(ValidationError::Invalid {
                field: "visibility",
                reason: "must be private or school",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the template is published to the whole school.
    pub fn is_school(&self) -> bool {
        self.0 == BANK_VISIBILITY_SCHOOL
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BankQuestionId(RecordId);

/// One `GROUP BY from_bank` row of [`BankQuestion::usage_counts`].
#[derive(SurrealValue)]
struct UsageCount {
    from_bank: BankQuestionId,
    n: i64,
}

/// What one [`BankQuestion::delete`] transaction removed — the template (an
/// empty list once someone else deleted it first) and the image rows swept
/// with it.
#[derive(SurrealValue)]
struct DeleteOutcome {
    question: Vec<BankQuestion>,
    images: Vec<BankQuestionImage>,
}

impl BankQuestionId {
    /// A write-ordered id. `list` sorts `id DESC` to mean "newest first", and
    /// a random ULID is only millisecond-accurate — templates saved inside one
    /// tick (a to-bank burst, a test loop) would come back shuffled, so the id
    /// comes from [`crate::domain::monotonic_id`] instead.
    pub fn generate() -> Self {
        Self(RecordId::new(BANK_QUESTION_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(BANK_QUESTION_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// One template in the shared question bank. `owner` is the teacher who saved
/// it (the only one who may edit or delete it, admins aside); `subject` is
/// origin metadata only — the same-course rule is checked against the caller's
/// subject when a template is copied into an exam, not here.
///
/// `subject` is optional because it is *only* metadata: deleting a subject
/// clears it school-wide (see [`crate::domain::subject::Subject::delete`])
/// rather than being blocked by the bank. Blocking would have been a dead end
/// — the bank is owner-or-admin editable, so a manager could not resolve their
/// own 409 — and an existence oracle, since another teacher's *private*
/// template would have raised it.
#[derive(Debug, Clone, SurrealValue)]
pub struct BankQuestion {
    id: BankQuestionId,
    owner: UserId,
    /// The origin subject, or `None` once that subject was deleted.
    /// `#[surreal(default)]` for the same reason `visibility` has one: rows
    /// written before the field went optional still decode.
    #[surreal(default)]
    subject: Option<SubjectId>,
    text: QuestionText,
    kind: QuestionKind,
    points: QuestionPoints,
    choices: Option<Vec<Choice>>,
    correct: Option<ChoiceId>,
    /// The exam question this template was saved from, if any.
    source_exam: Option<ExamId>,
    /// Who may read it. `#[surreal(default)]` (not serde — that doesn't compile
    /// here): rows written before the field existed decode as `private`.
    #[surreal(default)]
    visibility: BankVisibility,
    created_at: Timestamp,
}

impl BankQuestion {
    pub fn get_id(&self) -> &BankQuestionId {
        &self.id
    }

    pub fn get_owner(&self) -> &UserId {
        &self.owner
    }

    pub fn get_subject(&self) -> Option<&SubjectId> {
        self.subject.as_ref()
    }

    pub fn get_text(&self) -> &QuestionText {
        &self.text
    }

    pub fn get_kind(&self) -> &QuestionKind {
        &self.kind
    }

    pub fn get_points(&self) -> QuestionPoints {
        self.points
    }

    pub fn get_choices(&self) -> Option<&[Choice]> {
        self.choices.as_deref()
    }

    pub fn get_correct(&self) -> Option<&ChoiceId> {
        self.correct.as_ref()
    }

    pub fn get_source_exam(&self) -> Option<&ExamId> {
        self.source_exam.as_ref()
    }

    pub fn get_visibility(&self) -> &BankVisibility {
        &self.visibility
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// The stored kind-dependent fields as the validated bundle (for
    /// merge-on-update, and for copying into an exam). Bypasses `try_new`: the
    /// fields were written through a `QuestionSpec`, so the invariants already
    /// hold — and a round-trip through `try_new` would mint *new* choice ids,
    /// detaching the copy's `correct` and option pictures from its choices.
    pub fn spec(&self) -> QuestionSpec {
        QuestionSpec::from_stored(
            self.kind.clone(),
            self.choices.clone(),
            self.correct.clone(),
        )
    }

    pub async fn create(
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<BankQuestion, AppError> {
        Self::insert(owner, subject, text, points, spec, None, db).await
    }

    /// Like [`Self::create`], but records the origin exam the template was
    /// saved from (`POST …/questions/{qid}/to-bank`).
    pub async fn create_from_exam(
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source: ExamId,
        db: &Database,
    ) -> Result<BankQuestion, AppError> {
        Self::insert(owner, subject, text, points, spec, Some(source), db).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source_exam: Option<ExamId>,
        db: &Database,
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
        let created: Option<BankQuestion> =
            db.create(question.id.record()).content(question).await?;
        created.ok_or_else(|| AppError::Internal("failed to create bank question".into()))
    }

    pub async fn read(
        id: &BankQuestionId,
        db: &Database,
    ) -> Result<Option<BankQuestion>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// One page of the bank the caller may see, **newest first** (ULID ids sort by
    /// creation, so `id DESC` is newest-first) — plus the total row count under
    /// the same filters, so a client can page past the window.
    ///
    /// Every filter is a WHERE clause and the window is a real `LIMIT`/`START`:
    /// the old version read the whole table and sliced it in memory, which hid
    /// every template past the client's first page. `q` is a case- and
    /// diacritic-insensitive fragment of the question text (blank = no text
    /// filter): needle and column both go through
    /// [`crate::domain::text_fold`], so `istanbul` finds `İSTANBUL` and back.
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
    /// intersected with the gate leaves exactly the caller's own drafts. It
    /// joins the same `clauses` vec as the rest, so `total` honours it too —
    /// a count that ignored it would break paging.
    // Flat filter args, like every other `list` here — a builder struct for
    // four `Option`s and a window would be more machinery than the call sites.
    #[allow(clippy::too_many_arguments)]
    pub async fn list(
        visible_to: Option<&UserId>,
        owner: Option<&UserId>,
        subject: Option<&SubjectId>,
        visibility: Option<&BankVisibility>,
        q: Option<&str>,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<BankQuestion>, i64), AppError> {
        let needle = q.map(|q| search_fold(q.trim())).filter(|q| !q.is_empty());
        let mut clauses = Vec::new();
        if visible_to.is_some() {
            clauses.push("(visibility = 'school' OR owner = $viewer)");
        }
        if visibility.is_some() {
            clauses.push("visibility = $visibility");
        }
        if owner.is_some() {
            clauses.push("owner = $owner");
        }
        if subject.is_some() {
            clauses.push("subject = $subject");
        }
        let text_clause = format!("{} CONTAINS $q", search_fold_sql("text"));
        if needle.is_some() {
            clauses.push(text_clause.as_str());
        }
        let where_clause = if clauses.is_empty() {
            "true".to_string()
        } else {
            clauses.join(" AND ")
        };
        // `START` alone (no `LIMIT`) is the unpaged case — `?limit` is opt-in.
        let window = match limit {
            Some(_) => "LIMIT $limit START $offset",
            None => "START $offset",
        };
        // The count runs over the *same* WHERE, so `total` can never disagree
        // with what paging through the list actually yields.
        let mut query = db
            .query(format!(
                "SELECT * FROM bank_question WHERE {where_clause} ORDER BY id DESC {window};
                 SELECT VALUE count() FROM bank_question WHERE {where_clause} GROUP ALL;"
            ))
            .bind(("offset", offset));
        if let Some(viewer) = visible_to {
            query = query.bind(("viewer", viewer.record()));
        }
        if let Some(visibility) = visibility {
            query = query.bind(("visibility", visibility.as_str().to_string()));
        }
        if let Some(owner) = owner {
            query = query.bind(("owner", owner.record()));
        }
        if let Some(subject) = subject {
            query = query.bind(("subject", subject.record()));
        }
        if let Some(needle) = needle {
            query = query.bind(("q", needle));
        }
        if let Some(limit) = limit {
            query = query.bind(("limit", limit));
        }
        let mut result = query.await?.check()?;
        let questions = result.take::<Vec<BankQuestion>>(0)?;
        // `GROUP ALL` yields no row at all when nothing matched.
        let total = result.take::<Vec<i64>>(1)?.first().copied().unwrap_or(0);
        Ok((questions, total))
    }

    /// How many exam questions were instantiated from each of `ids` — the
    /// `from_bank` side of the provenance link, tallied for a whole page in
    /// **one** grouped query (a `count()` per row would be the N+1 this page
    /// already had removed once). Keys are template record keys; a template
    /// nobody ever used has no entry at all, so the caller reads a miss as
    /// zero — and the UI can stay quiet rather than print "0".
    ///
    /// Counts *questions*, not distinct exams: one exam that inserted the same
    /// template twice counts twice, which is what "copies made from this
    /// template" means and what the divergence trap is actually about.
    pub async fn usage_counts(
        ids: &[&BankQuestionId],
        db: &Database,
    ) -> Result<HashMap<String, i64>, AppError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let records: Vec<RecordId> = ids.iter().map(|id| id.record()).collect();
        let mut result = db
            .query(USAGE_COUNTS_SQL)
            .bind(("ids", records))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<UsageCount>>(0)?
            .into_iter()
            .map(|row| (row.from_bank.key().to_string(), row.n))
            .collect())
    }

    /// Write the editable fields of the template, but only while the row still
    /// reads as the snapshot `self` was loaded from — `None` means a concurrent
    /// edit (or the subject-delete cascade that clears `subject`) landed inside
    /// the handler's read-merge window, so nothing was written: reload,
    /// re-merge, retry. This is a genuine read-modify-write — every editable
    /// field is re-stated from the snapshot, and the kind/choices/correct trio
    /// has to be (a text-only edit re-submits the stored options *with their
    /// ids* so each keeps its picture) — so without the compare-and-set the
    /// later writer silently reverts the earlier one — and the window is a
    /// database round trip wide, which is exactly how far apart two concurrent
    /// requests to this one process can sit.
    ///
    /// Every field the merge re-states is in the guard, which is the same list
    /// the `SET` writes. `choices` is compared whole: its objects carry two
    /// required string keys, so none is ever dropped for being `NONE` (the trap
    /// [`crate::db::settings::save_if_unchanged`] works around).
    ///
    /// Rides [`write_with_retry`] because an image write now moves this row too
    /// ([`crate::domain::bank_question_image::BankQuestionImage::upsert`]): an
    /// upload landing in the same instant makes the store answer "conflict,
    /// retry", and a bare `check()` would turn that into a 500 for an edit that
    /// only had to be re-sent. A lost round writes nothing, and the guard is
    /// re-evaluated on the next one, so a genuine stale merge still comes back
    /// as `None`. Admissible: an `UPDATE` can never answer "already exists".
    pub async fn update_if_unchanged(
        self,
        subject: Option<SubjectId>,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        visibility: BankVisibility,
        db: &Database,
    ) -> Result<Option<BankQuestion>, AppError> {
        let (kind, choices, correct) = spec.into_parts();
        let rows: Vec<BankQuestion> = write_with_retry(
            db,
            "UPDATE $id SET subject = $subject, text = $text, points = $points,
             kind = $kind, choices = $choices, correct = $correct,
             visibility = $visibility
             WHERE subject = $was_subject AND text = $was_text
               AND points = $was_points AND kind = $was_kind
               AND choices = $was_choices AND correct = $was_correct
               AND visibility = $was_visibility
             RETURN AFTER",
            &[
                (
                    "was_subject".into(),
                    self.subject.clone().map(|s| s.record()).into_value(),
                ),
                ("was_text".into(), self.text.clone().into_value()),
                ("was_points".into(), self.points.into_value()),
                ("was_kind".into(), self.kind.clone().into_value()),
                ("was_choices".into(), self.choices.clone().into_value()),
                ("was_correct".into(), self.correct.clone().into_value()),
                (
                    "was_visibility".into(),
                    self.visibility.clone().into_value(),
                ),
                ("id".into(), self.id.record().into_value()),
                ("subject".into(), subject.map(|s| s.record()).into_value()),
                ("text".into(), text.into_value()),
                ("points".into(), points.into_value()),
                ("kind".into(), kind.into_value()),
                ("choices".into(), choices.into_value()),
                ("correct".into(), correct.into_value()),
                ("visibility".into(), visibility.into_value()),
            ],
        )
        .await?;
        Ok(rows.into_iter().next())
    }

    /// Delete the template and cascade-remove its bank images, so none points
    /// at a missing template. Bank rows have no answers. The image rows this
    /// actually removed come back with it: the blobs are the web layer's to
    /// take off disk, but only for *these* rows — an upload that committed
    /// after the caller listed the template's images is swept here too, and a
    /// pre-read snapshot would strand its blob for good
    /// ([`crate::db::note::delete`]'s story, this domain over).
    ///
    /// Children first, in one transaction: as two queries, a failure between
    /// them left image rows swept under a template that survived, or (the other
    /// order) a template gone with its images still there. Rides the retry
    /// because an image write now moves this very row
    /// ([`crate::domain::bank_question_image::BankQuestionImage::upsert`]) — the
    /// two contend by design, and a lost round through a bare `check()` would be
    /// a 500 for a delete that only had to be re-sent. Admissible: no `UPDATE`
    /// or `DELETE` in here can answer "already exists".
    ///
    /// Exam questions tied to this template keep living: only their provenance
    /// links are cleared, field-scoped (never a whole-row save — the question
    /// isn't ours and may be edited concurrently), so nothing can read a link to
    /// a template that no longer exists. *Both* directions point at a template,
    /// so both are cleared: `from_bank` on the questions instantiated from it,
    /// and `banked_as` on the question it was saved out of.
    pub async fn delete(
        self,
        db: &Database,
    ) -> Result<(BankQuestion, Vec<BankQuestionImage>), AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
             LET $images = (DELETE bank_question_image WHERE bank_question = $b RETURN BEFORE);
             UPDATE exam_question SET from_bank = NONE WHERE from_bank = $b;
             UPDATE exam_question SET banked_as = NONE WHERE banked_as = $b;
             LET $gone = (DELETE $b RETURN BEFORE);
             RETURN { question: $gone, images: $images };
             COMMIT TRANSACTION;",
            &[("b".into(), self.id.record().into_value())],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is the last statement before `COMMIT`, so its
        // slot follows the statement count rather than a hand-kept number;
        // `num_statements` counts BEGIN and COMMIT.
        let slot = result.num_statements().saturating_sub(2);
        let outcome = result
            .take::<Vec<DeleteOutcome>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to delete bank question".into()))?;
        let question = outcome
            .question
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)?;
        Ok((question, outcome.images))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        id: BankQuestionId,
        owner: UserId,
        subject: Option<SubjectId>,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source_exam: Option<ExamId>,
        visibility: BankVisibility,
        created_at: Timestamp,
    ) -> BankQuestion {
        let (kind, choices, correct) = spec.into_parts();
        BankQuestion {
            id,
            owner,
            subject,
            text,
            kind,
            points,
            choices,
            correct,
            source_exam,
            visibility,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> QuestionSpec {
        use crate::domain::exam_question::ChoiceInput;
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

    /// A row stored before `visibility` existed has no such key — it must decode
    /// as `private`, the safe value. Asserted on the decoder itself, so no
    /// schema DEFAULT can paper over it: a `school` default here would publish
    /// every pre-existing template in the school at once.
    #[tokio::test]
    async fn a_row_without_the_field_decodes_private() {
        use surrealdb::types::Value;

        let db = crate::database::init_mem().await.unwrap();
        let question = BankQuestion::create(
            UserId::generate(),
            SubjectId::generate(),
            QuestionText::try_new("q").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            &db,
        )
        .await
        .unwrap();
        assert_eq!(question.get_visibility().as_str(), BANK_VISIBILITY_PRIVATE);

        let mut stored = question.into_value();
        let Value::Object(ref mut map) = stored else {
            panic!("a bank question serializes to an object");
        };
        assert!(map.remove("visibility").is_some());
        let old = BankQuestion::from_value(stored).unwrap();
        assert_eq!(old.get_visibility().as_str(), BANK_VISIBILITY_PRIVATE);
        assert!(!old.get_visibility().is_school());
    }

    /// The visibility gate is a WHERE clause, so `total` counts exactly what the
    /// page can contain — never someone else's private templates.
    #[tokio::test]
    async fn list_hides_private_templates_from_others() {
        let db = crate::database::init_mem().await.unwrap();
        let owner = UserId::generate();
        let other = UserId::generate();
        let private = BankQuestion::create(
            owner.clone(),
            SubjectId::generate(),
            QuestionText::try_new("secret").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            &db,
        )
        .await
        .unwrap();
        let published = BankQuestion::create(
            owner.clone(),
            SubjectId::generate(),
            QuestionText::try_new("shared").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            &db,
        )
        .await
        .unwrap();
        let subject = published.get_subject().cloned();
        published
            .update_if_unchanged(
                subject,
                QuestionText::try_new("shared").unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap(),
                &db,
            )
            .await
            .unwrap();

        // The stranger sees the published one alone, and `total` agrees.
        let (items, total) = BankQuestion::list(Some(&other), None, None, None, None, None, 0, &db)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(items[0].get_text().as_str(), "shared");
        // The owner sees both; an admin (`None`) too.
        let (items, total) = BankQuestion::list(Some(&owner), None, None, None, None, None, 0, &db)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (2, 2));
        let (items, total) = BankQuestion::list(None, None, None, None, None, None, 0, &db)
            .await
            .unwrap();
        assert_eq!((items.len(), total), (2, 2));
        // An explicit `owner=` filter can't widen the gate.
        let (items, total) =
            BankQuestion::list(Some(&other), Some(&owner), None, None, None, None, 0, &db)
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
        let db = crate::database::init_mem().await.unwrap();
        let owner = UserId::generate();
        let stale = BankQuestion::create(
            owner,
            SubjectId::generate(),
            QuestionText::try_new("first").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            &db,
        )
        .await
        .unwrap();
        // Somebody else's edit lands on the row this snapshot came from.
        let landed = stale
            .clone()
            .update_if_unchanged(
                None,
                QuestionText::try_new("theirs").unwrap(),
                QuestionPoints::try_new(2).unwrap(),
                spec(),
                BankVisibility::try_new(BANK_VISIBILITY_PRIVATE).unwrap(),
                &db,
            )
            .await
            .unwrap();
        assert!(landed.is_some(), "the first write is on a fresh snapshot");

        let refused = stale
            .update_if_unchanged(
                None,
                QuestionText::try_new("mine").unwrap(),
                QuestionPoints::try_new(3).unwrap(),
                spec(),
                BankVisibility::try_new(BANK_VISIBILITY_PRIVATE).unwrap(),
                &db,
            )
            .await
            .unwrap();
        assert!(refused.is_none(), "a stale merge must not be written");
        let stored = BankQuestion::read(landed.unwrap().get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_text().as_str(), "theirs");
        assert_eq!(stored.get_points().as_i64(), 2);
    }

    /// The `visibility` filter ANDs onto the security gate, so it narrows and
    /// never widens: a stranger asking for `school` still can't see a private
    /// row, and `private` means "my own drafts" for everyone but an admin.
    #[tokio::test]
    async fn visibility_filter_narrows_never_widens() {
        let db = crate::database::init_mem().await.unwrap();
        let owner = UserId::generate();
        let other = UserId::generate();
        let mine = |text: &str| {
            BankQuestion::create(
                owner.clone(),
                SubjectId::generate(),
                QuestionText::try_new(text).unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                &db,
            )
        };
        mine("draft").await.unwrap();
        let published = mine("shared").await.unwrap();
        let subject = published.get_subject().cloned();
        published
            .update_if_unchanged(
                subject,
                QuestionText::try_new("shared").unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap(),
                &db,
            )
            .await
            .unwrap();
        let private = BankVisibility::try_new(BANK_VISIBILITY_PRIVATE).unwrap();
        let school = BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap();

        // Owner: `private` = their drafts, `school` = the published one.
        let (items, total) =
            BankQuestion::list(Some(&owner), None, None, Some(&private), None, None, 0, &db)
                .await
                .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(items[0].get_text().as_str(), "draft");
        let (items, total) =
            BankQuestion::list(Some(&owner), None, None, Some(&school), None, None, 0, &db)
                .await
                .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        assert_eq!(items[0].get_text().as_str(), "shared");
        // A stranger's `private` is empty — never someone else's draft.
        let (items, total) =
            BankQuestion::list(Some(&other), None, None, Some(&private), None, None, 0, &db)
                .await
                .unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);
        // …and their `school` stops at the published one, gate intact.
        let (items, total) =
            BankQuestion::list(Some(&other), None, None, Some(&school), None, None, 0, &db)
                .await
                .unwrap();
        assert_eq!((items.len(), total), (1, 1));
        // Composes with the other filters: right owner, wrong visibility.
        let (items, total) = BankQuestion::list(
            Some(&owner),
            Some(&owner),
            None,
            Some(&school),
            Some("draft"),
            None,
            0,
            &db,
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
        use crate::domain::exam_question::ExamQuestion;

        // One statement, so a page costs one round trip — a `;` here would mean
        // the per-row N+1 crept back in.
        assert!(
            !USAGE_COUNTS_SQL.contains(';'),
            "usage_counts must be one statement"
        );

        let db = crate::database::init_mem().await.unwrap();
        let owner = UserId::generate();
        let template = |text: &str| {
            BankQuestion::create(
                owner.clone(),
                SubjectId::generate(),
                QuestionText::try_new(text).unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                &db,
            )
        };
        let used_twice = template("twice").await.unwrap();
        let used_once = template("once").await.unwrap();
        let unused = template("never").await.unwrap();

        // Real exam rows: instantiating a template moves the exam's counter
        // (what keeps a question from outliving its exam), so a minted id
        // nothing wrote is a 404.
        let exam = crate::db::exam::published_exam(&db).await.get_id().clone();
        let other_exam = crate::db::exam::published_exam(&db).await.get_id().clone();
        // A real subject row: an exam question claims a reference on its
        // subject, so a minted id it never wrote would be refused.
        let subject = crate::domain::subject::Subject::create(
            &crate::db::course::a_test_course(&db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
            &db,
        )
        .await
        .unwrap();
        async fn instantiate(
            exam: &ExamId,
            subject: &SubjectId,
            source: &BankQuestion,
            db: &Database,
        ) {
            ExamQuestion::create_from_bank(
                exam,
                subject.clone(),
                source.get_text().clone(),
                source.get_points(),
                source.spec(),
                source.get_id().clone(),
                db,
            )
            .await
            .unwrap();
        }
        // Two copies of the same template, in two different exams.
        instantiate(&exam, subject.get_id(), &used_twice, &db).await;
        instantiate(&other_exam, subject.get_id(), &used_twice, &db).await;
        instantiate(&exam, subject.get_id(), &used_once, &db).await;
        // A hand-authored question has no `from_bank` and must not be tallied.
        ExamQuestion::create(
            &exam,
            subject.get_id().clone(),
            QuestionText::try_new("mine").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            &db,
        )
        .await
        .unwrap();

        let ids = [used_twice.get_id(), used_once.get_id(), unused.get_id()];
        let counts = BankQuestion::usage_counts(&ids, &db).await.unwrap();
        assert_eq!(counts.get(used_twice.get_id().key()).copied(), Some(2));
        assert_eq!(counts.get(used_once.get_id().key()).copied(), Some(1));
        assert_eq!(
            counts.get(unused.get_id().key()),
            None,
            "unused stays absent"
        );
        assert_eq!(counts.len(), 2);

        // A template outside the page is never counted into it.
        let narrow = BankQuestion::usage_counts(&[used_once.get_id()], &db)
            .await
            .unwrap();
        assert_eq!(narrow.len(), 1);
        assert_eq!(narrow.get(used_once.get_id().key()).copied(), Some(1));
        // An empty page asks nothing at all.
        assert!(
            BankQuestion::usage_counts(&[], &db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn visibility_rejects_anything_else() {
        assert!(BankVisibility::try_new("public").is_err());
        assert!(BankVisibility::try_new("").is_err());
        assert!(BankVisibility::try_new("school").unwrap().is_school());
    }

    #[tokio::test]
    async fn list_is_school_wide() {
        let db = crate::database::init_mem().await.unwrap();
        for _ in 0..2 {
            BankQuestion::create(
                UserId::generate(),
                SubjectId::generate(),
                QuestionText::try_new("q").unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                &db,
            )
            .await
            .unwrap();
        }
        // Different owners, yet both listed.
        let (items, total) = BankQuestion::list(None, None, None, None, None, None, 0, &db)
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(total, 2);
    }

    /// The window is SQL, not an in-memory slice, and `total` ignores it.
    #[tokio::test]
    async fn list_pages_newest_first_and_filters_text() {
        let db = crate::database::init_mem().await.unwrap();
        let owner = UserId::generate();
        for i in 0..5 {
            BankQuestion::create(
                owner.clone(),
                SubjectId::generate(),
                QuestionText::try_new(&format!("question {i}")).unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                &db,
            )
            .await
            .unwrap();
        }
        let (page, total) = BankQuestion::list(None, None, None, None, None, Some(2), 0, &db)
            .await
            .unwrap();
        assert_eq!(total, 5);
        assert_eq!(page.len(), 2);
        // Newest first: the last-created row leads.
        assert_eq!(page[0].get_text().as_str(), "question 4");
        // The tail is reachable by offset.
        let (tail, _) = BankQuestion::list(None, None, None, None, None, Some(2), 4, &db)
            .await
            .unwrap();
        assert_eq!(tail[0].get_text().as_str(), "question 0");
        // Text filter runs in SQL, case-insensitively, and narrows `total`.
        let (hits, total) =
            BankQuestion::list(None, None, None, None, Some("QUESTION 3"), None, 0, &db)
                .await
                .unwrap();
        assert_eq!((hits.len(), total), (1, 1));
        // Owner filter with nobody's templates.
        let (none, total) = BankQuestion::list(
            None,
            Some(&UserId::generate()),
            None,
            None,
            None,
            None,
            0,
            &db,
        )
        .await
        .unwrap();
        assert!(none.is_empty());
        assert_eq!(total, 0);
    }

    /// Turkish `İ`/`ı` must not split the search into two disjoint halves.
    #[tokio::test]
    async fn list_text_filter_folds_turkish_casing() {
        let db = crate::database::init_mem().await.unwrap();
        BankQuestion::create(
            UserId::generate(),
            SubjectId::generate(),
            QuestionText::try_new("İSTANBUL kaç ilçeye ayrılır?").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec(),
            &db,
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
            let (hits, total) =
                BankQuestion::list(None, None, None, None, Some(needle), None, 0, &db)
                    .await
                    .unwrap();
            assert_eq!((hits.len(), total), (1, 1), "needle {needle} missed");
        }
        let (miss, total) =
            BankQuestion::list(None, None, None, None, Some("ankara"), None, 0, &db)
                .await
                .unwrap();
        assert!(miss.is_empty());
        assert_eq!(total, 0);
    }
}
