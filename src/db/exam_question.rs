//! The `exam_question` table: row reads and listings, the answer-key join
//! behind review hiding, and the field-scoped writes that tie the freeze gate
//! and the subject's reference counter into their own transaction. The
//! workflows and the freeze pre-flight live in
//! [`crate::service::exam_question`].

use std::collections::HashSet;

use sqlx::PgConnection;

use crate::database::{Database, tx_with_retry};
use crate::db::exam_attempt::freeze_gate;
use crate::db::page::{PagedList, Param};
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    Choice, ChoiceId, ExamQuestion, ExamQuestionId, QuestionKind, QuestionPoints, QuestionSpec,
    QuestionText,
};
use crate::domain::subject::SubjectId;
use crate::error::{AppError, ValidationError};
use sqlx::types::Json;

/// The one answer for a subject that isn't there — a claim missing it and the
/// web layer's pre-flight lookup missing it are the same 400.
fn dead_subject() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "subject_id",
        reason: "subject does not exist",
    })
}

/// The `choices` JSONB bind as the macros type the parameter — a
/// `serde_json::Value`. A `Choice` is two plain strings: serializing one
/// cannot fail.
fn choices_as_value(choices: &[Choice]) -> serde_json::Value {
    serde_json::to_value(choices).expect("Choice serialization cannot fail")
}

pub async fn create(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<ExamQuestion, AppError> {
    insert(db, exam, subject, text, points, spec, None).await
}

/// Like [`create`], but records the bank template this question was
/// instantiated from (`POST …/questions/from-bank/{bid}`).
pub async fn create_from_bank(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    source: BankQuestionId,
) -> Result<ExamQuestion, AppError> {
    insert(db, exam, subject, text, points, spec, Some(source)).await
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    from_bank: Option<BankQuestionId>,
) -> Result<ExamQuestion, AppError> {
    let question = ExamQuestion {
        id: ExamQuestionId::generate(),
        exam: exam.clone(),
        subject,
        text,
        points,
        kind: spec.kind,
        choices: spec.choices.map(Json),
        correct: spec.correct,
        from_bank,
        // An insert never banks anything: only a to-bank save writes this.
        banked_as: None,
    };
    // The freeze gate and the subject's reference ride in the same
    // transaction as the insert: a question cannot appear under an exam
    // somebody has already started, and the claim that accounts for it
    // cannot outlive a row that never landed. A missed claim means the
    // subject is already gone — the same 400 the web layer's pre-flight
    // check answers with.
    //
    // The id is a freshly minted UUIDv7, so no rival can aim at it: the
    // only possible duplicate key is one this call minted, which is none.
    let exam = exam.clone();
    tx_with_retry(db, false, async move |conn| {
        freeze_gate(conn, &exam).await?;
        claim_subject_and_insert(conn, &question).await
    })
    .await
}

/// The subject's reference claim and the row insert as one statement — the
/// counter and the row commit together, and zero rows out means the subject
/// was already gone (the claim's `UPDATE` found nothing to increment). The
/// same verdict as the old `question_subject_gone` THROW, which nothing
/// else could reach: the insert is refused with its claim, atomically.
async fn claim_subject_and_insert(
    conn: &mut PgConnection,
    question: &ExamQuestion,
) -> Result<ExamQuestion, AppError> {
    let created = sqlx::query_as!(
        ExamQuestion,
        r#"WITH seat AS (
               UPDATE subject SET exam_question_count = subject.exam_question_count + 1
               WHERE id = $1
               RETURNING 1)
           INSERT INTO exam_question (id, exam, subject, text, kind, points,
                                      choices, correct, from_bank, banked_as)
           SELECT $2, $3, $1, $4, $5, $6, $7, $8, $9, NULL
           WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id AS "id: ExamQuestionId", exam AS "exam: ExamId",
                     subject AS "subject: SubjectId", text AS "text: QuestionText",
                     kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
                     choices AS "choices: Json<Vec<Choice>>",
                     correct AS "correct: ChoiceId",
                     from_bank AS "from_bank: BankQuestionId",
                     banked_as AS "banked_as: BankQuestionId""#,
        question.subject.uuid(),
        question.id.uuid(),
        question.exam.uuid(),
        question.text.as_str(),
        question.kind.as_str(),
        question.points.as_i64(),
        question
            .choices
            .as_ref()
            .map(|json| choices_as_value(&json.0)),
        question.correct.as_ref().map(ChoiceId::as_str),
        question.from_bank.as_ref().map(BankQuestionId::uuid),
    )
    .fetch_optional(&mut *conn)
    .await?;
    created.ok_or_else(dead_subject)
}

pub async fn read(db: &Database, id: &ExamQuestionId) -> Result<Option<ExamQuestion>, AppError> {
    Ok(sqlx::query_as!(
        ExamQuestion,
        r#"SELECT id AS "id: ExamQuestionId", exam AS "exam: ExamId",
                  subject AS "subject: SubjectId", text AS "text: QuestionText",
                  kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
                  choices AS "choices: Json<Vec<Choice>>",
                  correct AS "correct: ChoiceId",
                  from_bank AS "from_bank: BankQuestionId",
                  banked_as AS "banked_as: BankQuestionId"
           FROM exam_question WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?)
}

/// The exam's questions in presentation order (UUIDv7 ids sort by creation).
pub async fn list_for_exam(
    db: &Database,
    exam: &ExamId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ExamQuestion>, i64), AppError> {
    PagedList::new("exam_question WHERE exam = $1", "ORDER BY id ASC")
        .bind(Param::Uuid(exam.uuid()))
        .run::<ExamQuestion>(limit, offset, db)
        .await
}

/// The ids of `exam`'s questions that share a bank template with a question
/// under one of `live` — the questions two exams hold identical `correct`
/// for, because the bank copies the key into every instantiation. Keyed on
/// the template, never on the exam, so only the overlapping questions are
/// named. Empty when `live` is empty.
///
/// A question links to a template through *either* column and both must be
/// read, on both sides of the join: `from_bank` is the template it was
/// instantiated from, `banked_as` the template minted by saving it into the
/// bank ([`link_banked_as`]). A question authored by hand in exam A
/// and then saved to the bank holds only `banked_as`, while its copy in
/// exam B holds only `from_bank` — matching `from_bank` to `from_bank` saw
/// neither and leaked A's key while B was live. A single coalesced key per
/// row is not enough either: a question instantiated from one template and
/// re-saved as another carries *both*, and only the second one may be the
/// shared link.
///
/// So: collect every template id reachable from a live exam by either
/// column, then name any of `exam`'s questions pointing at one by either
/// column.
pub async fn list_shared_with(
    db: &Database,
    exam: &ExamId,
    live: &[ExamId],
) -> Result<HashSet<String>, AppError> {
    if live.is_empty() {
        return Ok(HashSet::new());
    }
    let live = live.iter().map(ExamId::uuid).collect::<Vec<_>>();
    let rows = sqlx::query!(
        r#"WITH shared AS (
               SELECT from_bank AS t FROM exam_question
               WHERE exam = ANY($1) AND from_bank IS NOT NULL
               UNION
               SELECT banked_as FROM exam_question
               WHERE exam = ANY($1) AND banked_as IS NOT NULL)
           SELECT q.id AS "id: ExamQuestionId" FROM exam_question q
           WHERE q.exam = $2
             AND (q.from_bank IN (SELECT t FROM shared)
                  OR q.banked_as IN (SELECT t FROM shared))"#,
        &live,
        exam.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.id.key()).collect())
}

/// Write the editable fields, refused outright once the exam has an
/// attempt — the freeze gate is part of this transaction, not a check the
/// caller made a moment earlier under a lock.
///
/// Field-scoped, no longer a whole-row save. The row also carries
/// `from_bank`/`banked_as`, which [`link_banked_as`] writes from a
/// *different* request: re-stating this snapshot's copy of them would
/// revert a to-bank save that landed in between. That is exactly what the
/// caller's old `EXAM_LOCK.write()` used to order (inside one process), and
/// naming the columns removes the need for any ordering at all.
pub async fn update(
    db: &Database,
    question: ExamQuestion,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<ExamQuestion, AppError> {
    // A re-tag moves a reference, and the move rides the very transaction
    // that moves the link: the old subject's release, the new one's claim
    // and the row write commit together or not at all, so no crash can
    // strand a count on a subject nothing points at (which would make it
    // undeletable forever).
    //
    // The two are armed on *different* conditions, which is the whole of
    // the rule: the counter statements only when the link actually
    // changes, but the CAS whenever this write *carries* the link — and it
    // always does, because the handler fills an omitted `subject_id` from
    // the row it read (`web::exams::questions`), so `subject` is in the
    // `SET` of every one of these updates. Arming the CAS on "changed"
    // instead would leave the re-stater through: a PATCH carrying the
    // subject its own stale snapshot held, sent while a rival's move
    // already landed, writes that stale subject straight back over the
    // winner — a revert with the counters left pointing at the move.
    let retag = (subject != question.subject).then_some((subject, question.subject));
    tx_with_retry(db, false, async move |conn| {
        // Freeze first, as always: it outranks every caller gate, and a
        // frozen exam answers the same 409 the pre-flight check gave.
        freeze_gate(conn, &question.exam).await?;
        if let Some((next, previous)) = &retag {
            sqlx::query!(
                r#"UPDATE subject SET exam_question_count =
                       GREATEST(exam_question_count - 1, 0)
                   WHERE id = $1"#,
                previous.uuid(),
            )
            .execute(&mut *conn)
            .await?;
            let claimed = sqlx::query!(
                r#"UPDATE subject SET exam_question_count = subject.exam_question_count + 1
                   WHERE id = $1 RETURNING 1 AS n"#,
                next.uuid(),
            )
            .fetch_optional(&mut *conn)
            .await?;
            if claimed.is_none() {
                return Err(dead_subject());
            }
        }
        // The CAS: the row write matches only while the question still sits on
        // the subject this handler read. A genuine no-op re-state passes it
        // trivially (the row holds exactly what is expected); a stale one —
        // whether it moves the link or restates it — matches nothing, aborts
        // the whole transaction, and so claims nothing and answers 409.
        let written = sqlx::query_as!(
            ExamQuestion,
            r#"UPDATE exam_question
               SET subject = $2, text = $3, points = $4, kind = $5,
                   choices = $6, correct = $7
               WHERE id = $1 AND subject = $8
               RETURNING id AS "id: ExamQuestionId", exam AS "exam: ExamId",
                         subject AS "subject: SubjectId", text AS "text: QuestionText",
                         kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
                         choices AS "choices: Json<Vec<Choice>>",
                         correct AS "correct: ChoiceId",
                         from_bank AS "from_bank: BankQuestionId",
                         banked_as AS "banked_as: BankQuestionId""#,
            question.id.uuid(),
            subject.uuid(),
            text.as_str(),
            points.as_i64(),
            spec.kind.as_str(),
            spec.choices.as_deref().map(choices_as_value),
            spec.correct.as_ref().map(ChoiceId::as_str),
            question.subject.uuid(),
        )
        .fetch_optional(&mut *conn)
        .await?;
        let Some(written) = written else {
            // The row is gone, or it moved under the snapshot. The same
            // 404 an empty write used to answer with distinguishes the
            // deleted-row case; anything still standing is a stale read,
            // which is the 409.
            let live = sqlx::query!(
                r#"SELECT EXISTS(SELECT 1 FROM exam_question WHERE id = $1) AS live"#,
                question.id.uuid(),
            )
            .fetch_one(&mut *conn)
            .await?;
            if !live.live.unwrap_or(false) {
                return Err(AppError::NotFound);
            }
            return Err(AppError::Conflict(
                "the subject this question was read on changed since; re-read and retry",
            ));
        };
        Ok(written)
    })
    .await
}

/// Point the question's `banked_as` at the bank template it was just saved
/// into (`POST …/questions/{qid}/to-bank`). The mirror direction of the
/// `from_bank` [`create_from_bank`] writes, and deliberately a
/// *different* column: a question inserted from the bank has not been saved
/// to it, and one field for both would make the client claim it was.
/// Overwrites any earlier link: repeat saves mint a new template and the
/// newest one wins.
///
/// Field-scoped write, unlike [`update`]: the caller awaits a bank
/// insert plus the whole blob-copy loop between reading this row and
/// linking it, so the row it holds is long stale by now — a whole-row save
/// would silently revert whatever landed in that window.
pub async fn link_banked_as(
    db: &Database,
    question: ExamQuestion,
    template: BankQuestionId,
) -> Result<ExamQuestion, AppError> {
    let linked = sqlx::query_as!(
        ExamQuestion,
        r#"UPDATE exam_question SET banked_as = $2 WHERE id = $1
           RETURNING id AS "id: ExamQuestionId", exam AS "exam: ExamId",
                     subject AS "subject: SubjectId", text AS "text: QuestionText",
                     kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
                     choices AS "choices: Json<Vec<Choice>>",
                     correct AS "correct: ChoiceId",
                     from_bank AS "from_bank: BankQuestionId",
                     banked_as AS "banked_as: BankQuestionId""#,
        question.id.uuid(),
        template.uuid(),
    )
    .fetch_optional(db)
    .await?;
    linked.ok_or(AppError::NotFound)
}

/// Delete the question and cascade-remove its answers and image rows, so
/// neither can point at a missing question. The image *blobs* are the web
/// layer's to remove — it collects their names before calling this.
/// Refused once the exam has an attempt, in the same transaction as the
/// delete — and the cascade now shares that transaction too, so a failure
/// mid-way can no longer strand answers whose question survived.
pub async fn delete(db: &Database, question: ExamQuestion) -> Result<ExamQuestion, AppError> {
    tx_with_retry(db, false, async move |conn| {
        freeze_gate(conn, &question.exam).await?;
        sqlx::query!(
            r#"DELETE FROM exam_answer WHERE question = $1"#,
            question.id.uuid(),
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            r#"DELETE FROM question_image WHERE question = $1"#,
            question.id.uuid(),
        )
        .execute(&mut *conn)
        .await?;
        let deleted = sqlx::query_as!(
            ExamQuestion,
            r#"DELETE FROM exam_question WHERE id = $1
               RETURNING id AS "id: ExamQuestionId", exam AS "exam: ExamId",
                         subject AS "subject: SubjectId", text AS "text: QuestionText",
                         kind AS "kind: QuestionKind", points AS "points: QuestionPoints",
                         choices AS "choices: Json<Vec<Choice>>",
                         correct AS "correct: ChoiceId",
                         from_bank AS "from_bank: BankQuestionId",
                         banked_as AS "banked_as: BankQuestionId""#,
            question.id.uuid(),
        )
        .fetch_optional(&mut *conn)
        .await?;
        // The subject's reference is given back inside this same transaction,
        // driven off what the delete actually removed — a question that wasn't
        // there decrements nothing.
        if let Some(deleted) = &deleted {
            sqlx::query!(
                r#"UPDATE subject SET exam_question_count =
                       GREATEST(exam_question_count - 1, 0)
                   WHERE id = $1"#,
                deleted.subject.uuid(),
            )
            .execute(&mut *conn)
            .await?;
        }
        deleted.ok_or(AppError::NotFound)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam_question::QuestionKind;

    fn kind(value: &str) -> QuestionKind {
        QuestionKind::try_new(value).unwrap()
    }

    /// The subject reference counter, which is what the subject's delete guard
    /// reads: every one of these asserts *stored* state, because a claim that
    /// outlives the row it accounts for makes its subject undeletable forever.
    mod counters {
        use super::*;
        use crate::database::init_test_db;
        use crate::domain::exam::{
            Exam, ExamAttemptLimit, ExamDescription, ExamKind, ExamMode, ExamSchedule, ExamTitle,
        };
        use crate::domain::settings::Settings;
        use crate::domain::subject::{Subject, SubjectDescription, SubjectName};
        use crate::domain::user::UserId;

        async fn an_exam(db: &Database) -> Exam {
            // The creator is a foreign key now: a real `app_user` row under
            // the fixture's fixed key.
            let creator = UserId::from_key("019732e3-7b00-7000-8000-00000000acdc");
            sqlx::query(
                "INSERT INTO app_user (id, username, created_at) \
                 VALUES ($1, 'acdc-fixture', 0)",
            )
            .bind(creator.uuid())
            .execute(db)
            .await
            .unwrap();
            let kinds = Settings::defaults().get_exam_kinds().to_vec();
            crate::db::exam::create(
                db,
                &creator,
                &crate::db::course::a_test_course(db).await,
                ExamTitle::try_new("practice").unwrap(),
                ExamDescription::try_new("").unwrap(),
                ExamKind::try_new("quiz", &kinds).unwrap(),
                ExamSchedule::try_new(Some(ExamMode::try_new("open").unwrap()), None, None, None)
                    .unwrap(),
                ExamAttemptLimit::try_new(1).unwrap(),
                true,
                false,
                false,
            )
            .await
            .unwrap()
        }

        async fn a_subject(db: &Database) -> Subject {
            crate::db::subject::create(
                db,
                &crate::db::course::a_test_course(db).await,
                SubjectName::try_new("topic").unwrap(),
                SubjectDescription::try_new("").unwrap(),
            )
            .await
            .unwrap()
        }

        /// The editable payload, fresh per call (each one is consumed).
        fn body() -> (QuestionText, QuestionPoints, QuestionSpec) {
            (
                QuestionText::try_new("3 + 3?").unwrap(),
                QuestionPoints::try_new(5).unwrap(),
                QuestionSpec::try_new(kind("text"), None, None, &[]).unwrap(),
            )
        }

        async fn a_question(exam: &Exam, on: &SubjectId, db: &Database) -> ExamQuestion {
            let (text, points, spec) = body();
            create(db, exam.get_id(), *on, text, points, spec)
                .await
                .unwrap()
        }

        async fn moved(
            question: ExamQuestion,
            to: &SubjectId,
            db: &Database,
        ) -> Result<ExamQuestion, AppError> {
            let (text, points, spec) = body();
            update(db, question, *to, text, points, spec).await
        }

        /// The stored `exam_question_count` on one subject, absent = zero.
        async fn count_on(subject: &SubjectId, db: &Database) -> i64 {
            sqlx::query_scalar::<_, i64>(
                "SELECT exam_question_count FROM subject WHERE id = $1",
            )
            .bind(subject.uuid())
            .fetch_one(db)
            .await
            .unwrap()
        }

        async fn rows(sql: &str, db: &Database) -> usize {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM ({sql}) AS t"
            )))
            .fetch_one(db)
            .await
            .unwrap() as usize
        }

        /// The freeze outranks the counter move, on both paths — and because
        /// the claim now rides the refused transaction, "outranks" has to mean
        /// the counters read as if nothing ran.
        #[tokio::test]
        async fn a_frozen_exam_leaves_the_subject_counters_untouched() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let (from, to) = (a_subject(&db).await, a_subject(&db).await);
            let (from, to) = (*from.get_id(), *to.get_id());
            let question = a_question(&exam, &from, &db).await;
            assert_eq!(count_on(&from, &db).await, 1);

            // A real user row: starting a sitting moves that student's badge
            // counter in the same transaction, and an `UPDATE` has nothing to
            // write to without one.
            let student = crate::db::user::create(
                &db,
                crate::domain::user::Username::try_new("ogrenci").unwrap(),
                None,
            )
            .await
            .unwrap();
            crate::service::exam_attempt::start(&db, &exam, student.get_id())
                .await
                .unwrap();

            let (text, points, spec) = body();
            let error = create(&db, exam.get_id(), to, text, points, spec)
                .await
                .expect_err("a started exam takes no new questions");
            assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
            assert_eq!(count_on(&to, &db).await, 0, "the refused claim rolled back");

            let error = moved(question, &to, &db)
                .await
                .expect_err("a started exam takes no re-tag either");
            assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
            assert_eq!(count_on(&from, &db).await, 1, "the release rolled back too");
            assert_eq!(count_on(&to, &db).await, 0);
        }

        /// The create-path invariant: a refused create writes neither the row
        /// nor a count — least of all on a subject it would have to invent.
        #[tokio::test]
        async fn a_create_on_a_dead_subject_writes_neither_row_nor_count() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let subject = a_subject(&db).await;
            let id = *subject.get_id();
            crate::db::subject::delete(&db, subject).await.unwrap();

            let (text, points, spec) = body();
            let error = create(&db, exam.get_id(), id, text, points, spec)
                .await
                .expect_err("a subject that is gone must not be taggable");
            assert!(error.to_string().contains("subject does not exist"));
            assert_eq!(
                rows("SELECT id FROM exam_question", &db).await,
                0,
                "a refused create may write no row"
            );
            assert_eq!(
                rows("SELECT id FROM subject", &db).await,
                0,
                "…and least of all a count on a subject it just brought back"
            );
        }

        #[tokio::test]
        async fn a_subject_move_moves_the_count() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let (from, to) = (a_subject(&db).await, a_subject(&db).await);
            let (from, to) = (*from.get_id(), *to.get_id());
            let question = a_question(&exam, &from, &db).await;

            let after = moved(question, &to, &db).await.unwrap();
            assert_eq!(after.get_subject(), &to);
            assert_eq!(count_on(&from, &db).await, 0, "the old subject is free");
            assert_eq!(count_on(&to, &db).await, 1, "the new one is not");
        }

        #[tokio::test]
        async fn a_move_to_a_dead_subject_leaves_everything_untouched() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let from = *a_subject(&db).await.get_id();
            let dead = a_subject(&db).await;
            let gone = *dead.get_id();
            crate::db::subject::delete(&db, dead).await.unwrap();
            let question = a_question(&exam, &from, &db).await;

            let error = moved(question.clone(), &gone, &db)
                .await
                .expect_err("a subject that is gone must not be taggable");
            assert!(error.to_string().contains("subject does not exist"));
            let stored = read(&db, question.get_id()).await.unwrap().unwrap();
            assert_eq!(stored.get_subject(), &from, "the link never moved");
            assert_eq!(
                count_on(&from, &db).await,
                1,
                "the release rolled back with the claim"
            );
            assert_eq!(rows("SELECT id FROM subject", &db).await, 1);
        }

        /// The double-claim guard. Both movers compute their claim and release
        /// from the row as *they* read it, so two PATCHes re-tagging the same
        /// question both release the old subject and both claim their own
        /// target — two counts for one link, and the loser's target is
        /// undeletable forever. The second call here runs on the struct read
        /// before the first one landed: it must be refused outright, and the
        /// counts must read as if it never ran.
        ///
        /// Drop `AND subject = $ref_expected` from the row write in
        /// [`update`] and this goes red on the very first
        /// assertion — the stale mover is happily applied.
        #[tokio::test]
        async fn a_stale_mover_is_refused_and_claims_nothing() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let from = *a_subject(&db).await.get_id();
            let to = *a_subject(&db).await.get_id();
            let other = *a_subject(&db).await.get_id();
            let question = a_question(&exam, &from, &db).await;
            let stale = question.clone();

            moved(question, &to, &db).await.unwrap();
            let error = moved(stale.clone(), &other, &db)
                .await
                .expect_err("a mover that read a subject it no longer holds must be refused");
            assert!(
                matches!(error, AppError::Conflict(_)),
                "a lost CAS is a conflict, not a 404 or a 500: {error:?}"
            );

            let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
            assert_eq!(stored.get_subject(), &to, "the winner's link");
            assert_eq!(count_on(&from, &db).await, 0, "released once, not twice");
            assert_eq!(count_on(&to, &db).await, 1, "claimed once");
            assert_eq!(count_on(&other, &db).await, 0, "never claimed");
        }

        /// The other half of the same race, and the one arming on "the subject
        /// changed" lets through: this PATCH carries the subject its snapshot
        /// already held, so it moves no counter — but a rival's move landed
        /// first, and writing that stale subject back would revert the winner
        /// while both counters still describe the move. The handler fills an
        /// omitted `subject_id` from the row it read, so this is also every
        /// text-only PATCH sent from a stale snapshot.
        ///
        /// Arm the CAS on the re-tag instead of on the write and this goes red
        /// on the first assertion — the re-state lands 200.
        #[tokio::test]
        async fn a_stale_re_stater_is_refused_and_reverts_nothing() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let from = *a_subject(&db).await.get_id();
            let to = *a_subject(&db).await.get_id();
            let question = a_question(&exam, &from, &db).await;
            let stale = question.clone();

            moved(question, &to, &db).await.unwrap();
            let error = moved(stale.clone(), &from, &db)
                .await
                .expect_err("re-stating a subject a rival moved off must be refused");
            assert!(
                matches!(error, AppError::Conflict(_)),
                "a lost CAS is a conflict, not a silent revert: {error:?}"
            );

            let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
            assert_eq!(stored.get_subject(), &to, "the winner's link stands");
            assert_eq!(
                count_on(&from, &db).await,
                0,
                "released once, and stayed so"
            );
            assert_eq!(count_on(&to, &db).await, 1, "claimed once, and stayed so");
        }

        /// …and the price of arming the CAS on every write is nothing: a PATCH
        /// re-stating the subject the row really holds passes it trivially.
        #[tokio::test]
        async fn a_no_op_re_state_still_lands() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let on = *a_subject(&db).await.get_id();
            let question = a_question(&exam, &on, &db).await;

            let (_, points, spec) = body();
            let after = update(
                &db,
                question,
                on,
                QuestionText::try_new("4 + 4?").unwrap(),
                points,
                spec,
            )
            .await
            .expect("re-stating the subject the row holds is not a conflict");
            assert_eq!(after.get_text().as_str(), "4 + 4?");
            assert_eq!(after.get_subject(), &on);
            assert_eq!(count_on(&on, &db).await, 1, "no counter moved");
        }

        /// A re-tag of a question whose row is gone is still the 404 it was,
        /// not the stale-mover 409 — the two empty-`$row` cases stay apart.
        #[tokio::test]
        async fn a_move_of_a_deleted_question_is_still_a_404() {
            let (db, _leases) = init_test_db().await;
            let exam = an_exam(&db).await;
            let from = *a_subject(&db).await.get_id();
            let to = *a_subject(&db).await.get_id();
            let question = a_question(&exam, &from, &db).await;
            delete(&db, question.clone()).await.unwrap();

            let error = moved(question, &to, &db)
                .await
                .expect_err("a deleted question cannot be re-tagged");
            assert!(matches!(error, AppError::NotFound), "{error:?}");
            assert_eq!(count_on(&to, &db).await, 0, "and nothing was claimed");
        }
    }
}
