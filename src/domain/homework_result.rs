//! A teacher's grade on one student's homework: a status
//! (`done`/`incomplete`/`missing`) plus an optional numeric [`Mark`] (0..=100,
//! reused from exam results). Like an exam result, the row id is the
//! deterministic `{homework}_{user}` composite, so grading is one atomic UPSERT
//! and there is exactly one grade per (homework, user) by construction.
//!
//! A stored result is what *freezes* a submission: while a grade exists the
//! student's submission and files are locked (the web layer answers 409), until
//! the teacher removes the grade to reopen them. The teacher-set `missing`
//! status is a deliberate verdict, distinct from the roster's *computed*
//! "missing" (unsubmitted past due) — the latter is derived in the web layer,
//! never stored.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    HOMEWORK_RESULT_TABLE, MARKS_GIVEN_TOTAL_FIELD, SUBMISSION_GRADED_FIELD, SUBMISSION_OPEN_GUARD,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::course::CourseId;
use crate::domain::exam_result::Mark;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_submission::HomeworkSubmissionId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_homework_status;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkResultId(RecordId);

impl HomeworkResultId {
    /// The one id a (homework, user) pair can have — a deterministic composite,
    /// so grading is a single atomic UPSERT with no find-then-insert race and
    /// one grade per pair by construction. ULID keys are alphanumeric, so `_`
    /// is an unambiguous joiner.
    pub fn composite(homework: &HomeworkId, user: &UserId) -> Self {
        Self(RecordId::new(
            HOMEWORK_RESULT_TABLE,
            format!("{}_{}", homework.key(), user.key()),
        ))
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

/// A validated homework grade: `done` (submitted and complete), `incomplete`
/// (submitted but lacking), or `missing` (not done). Held to the
/// [`crate::constant::HOMEWORK_STATUSES`] set by [`HomeworkStatus::try_new`] —
/// the same validated-string-against-a-const shape as
/// [`crate::domain::exam::ExamMode`], which is why the `status` column needs no
/// DDL `ASSERT` (repo convention: enums live in Rust newtypes, not the schema).
/// It stores as the bare string. A stored `missing` is a teacher's deliberate
/// verdict, distinct from the roster's *computed* missing (unsubmitted past due,
/// derived in the web layer, never stored).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkStatus(String);

impl HomeworkStatus {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_homework_status(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct HomeworkResult {
    id: HomeworkResultId,
    homework: HomeworkId,
    user: UserId,
    status: HomeworkStatus,
    mark: Option<Mark>,
    graded_by: UserId,
    created_at: Timestamp,
}

impl HomeworkResult {
    pub fn get_id(&self) -> &HomeworkResultId {
        &self.id
    }

    pub fn get_homework(&self) -> &HomeworkId {
        &self.homework
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_status(&self) -> &HomeworkStatus {
        &self.status
    }

    /// The optional numeric mark; `None` when the teacher graded status only.
    pub fn get_mark(&self) -> Option<Mark> {
        self.mark
    }

    pub fn get_graded_by(&self) -> &UserId {
        &self.graded_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Record (or overwrite) the grade for (homework, user). One row per pair,
    /// keyed by the deterministic composite id, so this is a single atomic
    /// UPSERT — concurrent grades for the same pair converge on one row instead
    /// of racing a unique index into a 500. Grading before the due date, or
    /// before any submission exists, is allowed (the caller's policy call).
    ///
    /// In the same transaction the grade *stamps* the student's submission
    /// ([`crate::constant::SUBMISSION_GRADED_FIELD`]), which is what freezes it:
    /// every student-side write to that row then fails its own
    /// `graded_by_result = NONE` condition, with no cross-table read for a
    /// concurrent write to slip past. A submission that does not exist yet is
    /// left alone — grading absent work must not conjure a hand-in (the report reads
    /// `submitted`/`missing`/`late` straight off that row).
    ///
    /// That one case is where this function stops defending itself: a student's
    /// *first* hand-in committing between this grade and nothing-to-stamp would
    /// land unstamped, leaving a grade beside an editable submission. Nothing
    /// here forbids it — what forbids it is the caller. Grading holds
    /// `HOMEWORK_LOCK` (see `crate::web::homework`) for writing across this
    /// whole transaction while every student-side write holds it for reading across
    /// its own, and the two leases are mutually exclusive in the one process
    /// this backend runs as, so the interleaving never gets a window.
    //
    // corner-cut: that makes the freeze depend on lock discipline at the call
    // sites, not on this row. Moving a student write's lease to *after* its
    // database write — or sharding HOMEWORK_LOCK per homework — reopens the
    // window with nothing to catch it. Closing it in the domain layer needs a
    // record both sides own (a per-(homework, user) row the grade can always
    // stamp); a tombstone submission is not it, since every read path would
    // have to learn to ignore it and one missed filter fabricates a hand-in.
    pub async fn grade(
        homework: &HomeworkId,
        user: &UserId,
        status: HomeworkStatus,
        mark: Option<Mark>,
        graded_by: &UserId,
        db: &Database,
    ) -> Result<HomeworkResult, AppError> {
        let id = HomeworkResultId::composite(homework, user);
        let submission = HomeworkSubmissionId::composite(homework, user);
        let result = HomeworkResult {
            id: id.clone(),
            homework: homework.clone(),
            user: user.clone(),
            status,
            mark,
            graded_by: graded_by.clone(),
            created_at: Timestamp::now(),
        };
        // Whether the pair was already graded is read one statement ahead of
        // the write, inside the same transaction — it is what keeps a regrade
        // from crediting the grader a second time. No mark counter here: a
        // homework mark is optional and most grades are status-only, so
        // `high_mark` is an exam-only family.
        //
        // The first three statements are the parent gate, the twin of
        // [`crate::domain::homework_submission::HomeworkSubmission::upsert`]'s
        // and written the same way on purpose: the id is deterministic, so a
        // grade landing inside a homework (or course) delete's window does not
        // fail — it *re-creates* a result row under a homework that is gone,
        // readable ever after at `GET /homework/{id}/result` (no existence check
        // there) with a `marks_given_total` no ungrade can reach, because every
        // route to the grade goes through the homework. Reading the homework
        // first — which the web layer does, under the lease — cannot close that:
        // the store conflict-checks write sets, not read sets. So the gate
        // *moves* a value on the homework row (`due_at` up by one and straight
        // back to the captured value, leaving the row byte-identical) and the
        // cascade's delete of that same row is what refuses it. `homework` owns
        // no counter column and is SCHEMAFULL, so `due_at` is the one `int` it
        // already has; do not invent a second spelling.
        //
        // Not [`crate::db::cap::touch_and_create`], which is the same shape
        // for a bare `CREATE`: a grade is an UPSERT (a regrade must overwrite),
        // and the freeze stamp and the grader's counter have to ride the same
        // transaction, which that helper has no room for.
        //
        // Sound to re-send, which is what the gate needed first — a `THROW`
        // inside a plain `db.query` would have turned every lost round into a
        // 500. `SELECT`, `UPDATE` and the `IF`/`THROW` can never answer "already
        // exists", and the `UPSERT`'s id is bijective with the (homework, user)
        // pair the table keys, on a table whose only index is non-unique
        // (`homework_result_homework`), so its index entry can only ever point
        // at the row the id already names: it resolves onto that row instead of
        // colliding with it. A lost round wrote nothing.
        let (mut saved, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $was_due = (SELECT VALUE due_at FROM ONLY $hw);
                 LET $alive = (UPDATE $hw SET due_at = due_at + 1 RETURN VALUE id);
                 IF array::len($alive) = 0 {{ THROW 'no_homework' }};
                 UPDATE $hw SET due_at = $was_due;
                 LET $before = (SELECT VALUE id FROM ONLY $id);
                 LET $after = (UPSERT $id CONTENT $row RETURN AFTER);
                 UPDATE $sub SET {SUBMISSION_GRADED_FIELD} = $id \
                     WHERE {SUBMISSION_OPEN_GUARD};
                 IF $before = NONE {{
                     UPDATE $grader SET {MARKS_GIVEN_TOTAL_FIELD} =
                         ({MARKS_GIVEN_TOTAL_FIELD} ?? 0) + 1
                 }};
                 RETURN $after;
                 COMMIT TRANSACTION;"
            ),
            &[
                ("hw".into(), homework.record().into_value()),
                ("id".into(), id.record().into_value()),
                ("sub".into(), submission.record().into_value()),
                ("grader".into(), graded_by.record().into_value()),
                ("row".into(), result.into_value()),
            ],
            &["no_homework"],
        )
        .await?;
        // An aborted transaction errors *every* slot, most with a generic "not
        // executed" — only the THROW's own slot names the reason.
        if errors
            .values()
            .any(|error| error.to_string().contains("no_homework"))
        {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`,
        // so its slot follows the statement count instead of a hand-kept
        // number — which the gate above would otherwise have shifted, silently
        // handing back something else.
        let slot = saved.num_statements().saturating_sub(2);
        saved
            .take::<Vec<HomeworkResult>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to record homework result".into()))
    }

    /// `user`'s grade for `homework`, if graded.
    pub async fn read_for(
        homework: &HomeworkId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<HomeworkResult>, AppError> {
        Ok(db
            .select(HomeworkResultId::composite(homework, user).record())
            .await?)
    }

    /// Every grade for `homework` — the roster joins these onto the submissions.
    pub async fn list_for_homework(
        homework: &HomeworkId,
        db: &Database,
    ) -> Result<Vec<HomeworkResult>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework_result WHERE homework = $hw ORDER BY id DESC")
            .bind(("hw", homework.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<HomeworkResult>>(0)?)
    }

    /// `user`'s homework grades across one course — the raw rows behind the
    /// per-course block of a homework report. Mirrors
    /// `ExamResult::list_for_user_in_course`.
    pub async fn list_for_user_in_course(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<HomeworkResult>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM homework_result WHERE user = $usr
                 AND homework IN (SELECT VALUE id FROM homework WHERE course = $course)
                 ORDER BY id DESC",
            )
            .bind(("usr", user.record()))
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<HomeworkResult>>(0)?)
    }

    /// Un-grade (homework, user), returning the removed row (`None` if there
    /// was none). Removing the grade unfreezes the student's submission, so the
    /// stamp [`grade`](Self::grade) left on it is cleared in the same
    /// transaction — scoped to *this* grade's id, so it can never wipe a stamp a
    /// concurrent re-grade has just written.
    ///
    /// The grader's `marks_given_total` comes back with it, in that same
    /// transaction and only when a row was really deleted. [`grade`](Self::grade)
    /// credits on the branch that finds no row, and a deleted row *is* no row:
    /// left standing, ungrade-then-regrade credited a second time off one
    /// stored grade, and looped it minted a badge with no exam, no mark and no
    /// submission behind it — permanently, since an award is never revoked.
    /// Given back to the *removed row's own* grader, not the caller: a manager
    /// may un-grade what a teacher graded, and the credit is the teacher's.
    ///
    /// Sound to re-send while the store answers "conflict, retry" — this is the
    /// upgrade `grade` still wants, taken here because the counter above is what
    /// makes it necessary: the grader's row is written by exam grading too, so
    /// the delete now contends with another domain, and only `DELETE`/`UPDATE`
    /// are in the batch, none of which can legitimately answer "already exists".
    pub async fn remove(
        homework: &HomeworkId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<HomeworkResult>, AppError> {
        let id = HomeworkResultId::composite(homework, user);
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE $id RETURN BEFORE);
                 UPDATE $sub SET {SUBMISSION_GRADED_FIELD} = NONE \
                     WHERE {SUBMISSION_GRADED_FIELD} = $id;
                 IF array::len($gone) > 0 {{
                     LET $grader = $gone[0].graded_by;
                     UPDATE $grader SET {MARKS_GIVEN_TOTAL_FIELD} =
                         math::max([({MARKS_GIVEN_TOTAL_FIELD} ?? 0) - 1, 0])
                 }};
                 RETURN $gone;
                 COMMIT TRANSACTION;"
            ),
            &[
                ("id".into(), id.record().into_value()),
                (
                    "sub".into(),
                    HomeworkSubmissionId::composite(homework, user)
                        .record()
                        .into_value(),
                ),
            ],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`, so
        // its slot follows the statement count — the hand-kept "slot 1" this
        // replaces was one added statement away from handing back the unstamp.
        let slot = result.num_statements().saturating_sub(2);
        Ok(result.take::<Vec<HomeworkResult>>(slot)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::HOMEWORK_STATUSES;
    use surrealdb::types::Value;

    /// A real homework row (with the course and subject it needs): the grade now
    /// carries a parent gate, so a minted id nothing wrote is a `404` here, the
    /// same way it already is for a submission.
    async fn a_homework(db: &Database) -> HomeworkId {
        use crate::domain::homework::{Homework, HomeworkTitle};
        use crate::domain::subject::{SubjectDescription, SubjectName};

        let course = crate::db::course::a_test_course(db).await;
        let subject = crate::db::subject::create(
            db,
            &course,
            SubjectName::try_new("topic").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        Homework::create(
            &course,
            subject.get_id(),
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
            db,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    #[tokio::test]
    async fn status_is_held_to_the_const_and_stores_as_a_bare_string() {
        // Enforcement lives in the newtype against HOMEWORK_STATUSES (the DDL
        // carries no ASSERT), and the stored value is the bare validated string
        // the `status` column's `TYPE string` accepts. Guard both never drift.
        for status in HOMEWORK_STATUSES {
            let parsed = HomeworkStatus::try_new(status).unwrap();
            assert_eq!(parsed.as_str(), status);
            assert_eq!(parsed.into_value(), Value::String(status.to_string()));
        }
        assert!(HomeworkStatus::try_new("late").is_err());
        assert!(HomeworkStatus::try_new("").is_err());
    }

    #[tokio::test]
    async fn grade_upserts_one_row_per_pair_and_remove_unfreezes() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(&db).await;
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");

        let first = HomeworkResult::grade(
            &homework,
            &user,
            HomeworkStatus::try_new("incomplete").unwrap(),
            None,
            &teacher,
            &db,
        )
        .await
        .unwrap();
        // A regrade lands on the same row (composite id), overwriting the grade.
        let second = HomeworkResult::grade(
            &homework,
            &user,
            HomeworkStatus::try_new("done").unwrap(),
            Some(Mark::try_new(80).unwrap()),
            &teacher,
            &db,
        )
        .await
        .unwrap();
        assert_eq!(first.get_id(), second.get_id());
        assert_eq!(second.get_status().as_str(), "done");
        assert_eq!(second.get_mark().unwrap().as_i64(), 80);
        assert_eq!(
            HomeworkResult::list_for_homework(&homework, &db)
                .await
                .unwrap()
                .len(),
            1
        );

        assert!(
            HomeworkResult::read_for(&homework, &user, &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            HomeworkResult::remove(&homework, &user, &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            HomeworkResult::read_for(&homework, &user, &db)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The grader is credited once per graded pair: a regrade lands on the same
    /// row, so it is not a mark given twice. The student's counters are not
    /// touched at all — a homework grade is not a homework submission.
    #[tokio::test]
    async fn a_first_grade_credits_the_grader_and_a_regrade_does_not() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(&db).await;
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        // `UPDATE` writes nothing to a user row that does not exist.
        db.query("CREATE $usr SET username = 't', password_hash = 'x'")
            .bind(("usr", teacher.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        for status in ["incomplete", "done"] {
            HomeworkResult::grade(
                &homework,
                &user,
                HomeworkStatus::try_new(status).unwrap(),
                None,
                &teacher,
                &db,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            marks_given(&teacher, &db).await,
            1,
            "one graded pair, one mark given"
        );
    }

    /// Un-grading gives the credit back, so grade → ungrade → regrade nets to
    /// one however often it is run. Crediting the grade without refunding the
    /// delete was the cheapest counter farm in the codebase — two requests per
    /// mark given, with no exam, no mark and no submission behind them.
    #[tokio::test]
    async fn ungrading_gives_the_grader_credit_back() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(&db).await;
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        db.query("CREATE $usr SET username = 't', password_hash = 'x'")
            .bind(("usr", teacher.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        for _ in 0..3 {
            HomeworkResult::grade(
                &homework,
                &user,
                HomeworkStatus::try_new("missing").unwrap(),
                None,
                &teacher,
                &db,
            )
            .await
            .unwrap();
            assert_eq!(marks_given(&teacher, &db).await, 1);
            assert!(
                HomeworkResult::remove(&homework, &user, &db)
                    .await
                    .unwrap()
                    .is_some(),
                "the grade was there"
            );
            assert_eq!(marks_given(&teacher, &db).await, 0, "the loop kept one");
        }

        // Nothing to remove is nothing to give back — the floor holds.
        assert!(
            HomeworkResult::remove(&homework, &user, &db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(marks_given(&teacher, &db).await, 0);
    }

    /// The grader's badge counter, re-read out of the store.
    async fn marks_given(user: &UserId, db: &Database) -> i64 {
        let mut result = db
            .query(format!(
                "SELECT VALUE ({MARKS_GIVEN_TOTAL_FIELD} ?? 0) FROM $usr"
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or(0)
    }
}
