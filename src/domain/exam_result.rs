use std::collections::HashMap;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    EXAM_RESULT_COUNT_FIELD, EXAM_RESULT_TABLE, KIND_REF_TABLE, REF_COUNT_FIELD,
};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::key;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_mark;

/// The one spelling of "this exam is hidden", shared by the grade handler's
/// pre-flight gate and the in-transaction guard on the mark write.
pub(crate) fn draft_error() -> AppError {
    AppError::Conflict("this exam is a draft — publish it before grading")
}

/// The reference counter for one exam kind — how many marks are written under
/// that name, and whether the school has retired it (see
/// [`crate::domain::cap`]). Keyed by the name itself: the kind is snapshotted
/// text on the exam, and this row is what makes "a kind nothing is graded under
/// may be removed" a decision the database takes, not a count a concurrent mark
/// can invalidate.
pub(crate) fn kind_ref(kind: &str) -> RecordId {
    RecordId::new(KIND_REF_TABLE, kind)
}

/// The refusal a mark meets once its kind has left the school's list. The
/// mirror of the settings-side 409: whichever of the two writes reaches the
/// counter first, the other is told the name is no longer usable.
pub(crate) fn retired_kind_error(kind: &str) -> AppError {
    AppError::ConflictOwned(format!(
        "the '{kind}' exam kind has been removed from the school's settings — \
         add it back before grading this exam"
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamResultId(RecordId);

impl ExamResultId {
    /// A deterministic id for the (exam, user, seq) triple — one mark row per
    /// sitting, so grading is a single atomic UPSERT with no find-then-insert
    /// race. See [`key::sitting`] for the key shape and why the first sitting
    /// stays bare.
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        let key = key::sitting(exam.key(), user.key(), seq);
        Self(RecordId::new(EXAM_RESULT_TABLE, key))
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

/// A validated exam mark. Stored as an `int`, held to `[MIN_MARK, MAX_MARK]`.
/// Kept in its own row — an exam result is never a course note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct Mark(i64);

impl Mark {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_mark(value)?;
        Ok(Self(value))
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }
}

/// What one mark write reports back: the stored row, and whether it replaced a
/// mark that was already there.
#[derive(Debug, Clone, SurrealValue)]
struct Written {
    existed: bool,
    result: ExamResult,
}

#[derive(Debug, Clone, SurrealValue)]
pub struct ExamResult {
    id: ExamResultId,
    exam: ExamId,
    user: UserId,
    seq: i64,
    mark: Mark,
    graded_by: UserId,
}

impl ExamResult {
    pub fn get_id(&self) -> &ExamResultId {
        &self.id
    }

    pub fn get_exam(&self) -> &ExamId {
        &self.exam
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    /// Which sitting this mark grades — 1 for the first attempt, counting up.
    pub fn get_seq(&self) -> i64 {
        self.seq
    }

    pub fn get_mark(&self) -> Mark {
        self.mark
    }

    pub fn get_graded_by(&self) -> &UserId {
        &self.graded_by
    }

    async fn find(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
        // Grade-of-record is the latest sitting's mark: the highest seq wins.
        let mut result = db
            .query(
                "SELECT * FROM exam_result WHERE exam = $ex AND user = $usr
                 ORDER BY seq DESC LIMIT 1",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamResult>>(0)?.into_iter().next())
    }

    /// The latest-seq mark per (exam, user) pair, preserving the outer list's
    /// row order (first appearance of each pair). Grade-of-record is the latest
    /// attempt's mark, so a pair with retakes collapses to its highest seq. Done
    /// in Rust rather than SQL: SurrealDB's `GROUP BY` can't return the whole
    /// row that carries the max, and id-order can't stand in for seq-order once
    /// seq reaches two digits.
    fn latest_per_pair(rows: Vec<ExamResult>) -> Vec<ExamResult> {
        let mut order: Vec<(String, String)> = Vec::new();
        let mut best: HashMap<(String, String), ExamResult> = HashMap::new();
        for row in rows {
            let key = (row.exam.key().to_string(), row.user.key().to_string());
            match best.get(&key) {
                Some(existing) if existing.seq >= row.seq => {}
                Some(_) => {
                    best.insert(key, row);
                }
                None => {
                    order.push(key.clone());
                    best.insert(key, row);
                }
            }
        }
        order
            .into_iter()
            .map(|key| best.remove(&key).expect("key was recorded on insert"))
            .collect()
    }

    /// A single user's result for an exam, if graded.
    pub async fn read_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
        Self::find(exam, user, db).await
    }

    /// Record (or overwrite) `user`'s mark for the `seq`th sitting of `exam`.
    /// One row per (exam, user, seq), keyed by a deterministic composite id so
    /// this is a single atomic UPSERT — concurrent grades for the same sitting
    /// converge on one row instead of racing the unique index into a 500.
    /// Grading a retake writes a fresh mark at the current seq and never touches
    /// prior sittings' marks; the latest seq is the grade-of-record.
    ///
    /// `kind` is the exam's kind, and this is where a mark takes its reference
    /// on it — claimed *before* the write, given back when the write turns out
    /// to have been an overwrite (one mark, one reference) or to have failed. A
    /// kind the school has retired refuses the claim, which is the same
    /// invariant the settings guard enforces from the other side, and the one
    /// crash window left over-counts a kind (refusing a removal) rather than
    /// letting a mark exist under a kind nothing counted.
    ///
    /// The exam's own `result_count` is claimed in the same breath, and it is
    /// what a kind change is refused against: the exam PATCH pins that counter,
    /// so a mark landing while it decides cannot slip past its gate and leave
    /// itself counted under a kind its exam no longer carries.
    pub async fn grade(
        exam: &ExamId,
        user: &UserId,
        seq: i64,
        mark: Mark,
        graded_by: &UserId,
        kind: &str,
        db: &Database,
    ) -> Result<ExamResult, AppError> {
        let counter = kind_ref(kind);
        if !cap::claim_ref(&counter, 1, db).await? {
            return Err(retired_kind_error(kind));
        }
        cap::claim(&exam.record(), EXAM_RESULT_COUNT_FIELD, cap::UNLIMITED, db).await?;
        let written = Self::write_mark(exam, user, seq, mark, graded_by, db).await;
        match written {
            // An overwrite is not a second mark: both counters go back, or a
            // regrade would drift them upward and freeze the kind for good.
            Ok(Written { existed: true, .. }) | Err(_) => {
                cap::release_ref(&counter, 1, db).await?;
                cap::release(&exam.record(), EXAM_RESULT_COUNT_FIELD, db).await?;
            }
            Ok(_) => {}
        }
        written.map(|written| written.result)
    }

    async fn write_mark(
        exam: &ExamId,
        user: &UserId,
        seq: i64,
        mark: Mark,
        graded_by: &UserId,
        db: &Database,
    ) -> Result<Written, AppError> {
        let result = ExamResult {
            id: ExamResultId::composite(exam, user, seq),
            exam: exam.clone(),
            user: user.clone(),
            seq,
            mark,
            graded_by: graded_by.clone(),
        };
        // The "not a draft" gate rides in the same transaction as the mark, the
        // mirror of the re-draft gate on `Exam::update_if_unchanged`: between
        // them, a mark and a re-draft racing each other can only ever leave one
        // of the two applied, whichever process either ran in. The caller's
        // pre-flight check answers the same 409 one round trip earlier.
        // Whether the row was already there rides out of the transaction with
        // the mark: the answer decides if this grade owes the kind a reference,
        // and read anywhere else it would be a guess about a row two graders
        // may be writing at once.
        let mut written = db
            .query(
                "BEGIN TRANSACTION;
                 IF (SELECT VALUE draft FROM ONLY $exam) { THROW 'exam_draft' };
                 LET $before = (SELECT VALUE id FROM ONLY $id);
                 LET $after = (UPSERT $id CONTENT $result RETURN AFTER);
                 RETURN { existed: $before != NONE, result: $after[0] };
                 COMMIT TRANSACTION;",
            )
            .bind(("exam", exam.record()))
            .bind(("id", result.id.record()))
            .bind(("result", result))
            .await?;
        let mut errors = written.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains("exam_draft"))
        {
            return Err(draft_error());
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`,
        // so its slot follows the statement count instead of a hand-kept
        // number — see `Exam::delete` for the bug the hand-kept one caused.
        let slot = written.num_statements().saturating_sub(2);
        written
            .take::<Vec<Written>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to record exam result".into()))
    }

    /// The user's graded results restricted to one course's exams — the raw
    /// rows behind the per-course block of the marks report.
    pub async fn list_for_user_in_course(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<ExamResult>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM exam_result WHERE user = $usr
                 AND exam IN (SELECT VALUE id FROM exam WHERE course = $course)
                 ORDER BY id DESC",
            )
            .bind(("usr", user.record()))
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(Self::latest_per_pair(result.take::<Vec<ExamResult>>(0)?))
    }

    pub async fn list_for_exam(exam: &ExamId, db: &Database) -> Result<Vec<ExamResult>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_result WHERE exam = $ex ORDER BY id DESC")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(Self::latest_per_pair(result.take::<Vec<ExamResult>>(0)?))
    }

    /// Every sitting's mark for one (exam, user) pair, oldest first — the
    /// per-attempt grade history behind the FE's attempt-by-attempt view.
    pub async fn list_all_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<ExamResult>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM exam_result WHERE exam = $ex AND user = $usr
                 ORDER BY seq ASC",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamResult>>(0)?)
    }

    /// Delete every sitting's mark for one (exam, user) pair. `kind` is the
    /// exam's: the marks give their references back, so a kind nothing is
    /// graded under any more can leave the settings again.
    pub async fn remove(
        exam: &ExamId,
        user: &UserId,
        kind: &str,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
        // Both counters go back *inside* the delete's own transaction, driven
        // off the rows this statement actually deleted. Counted outside it, a
        // cascade (exam or course delete) taking the same rows in the gap would
        // release them a second time, and on a kind another exam still grades
        // under, one release too many reads as one mark too few — a kind
        // wrongly free to leave the settings.
        let mut result = db
            .query(format!(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE exam_result WHERE exam = $ex AND user = $usr RETURN BEFORE);
                 IF array::len($gone) > 0 {{
                     UPDATE type::record('kind_ref', $kind) SET {REF_COUNT_FIELD} =
                         math::max([({REF_COUNT_FIELD} ?? 0) - array::len($gone), 0]);
                     UPDATE $ex SET {EXAM_RESULT_COUNT_FIELD} =
                         math::max([({EXAM_RESULT_COUNT_FIELD} ?? 0) - array::len($gone), 0]);
                 }};
                 RETURN $gone;
                 COMMIT TRANSACTION;"
            ))
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .bind(("kind", kind.to_string()))
            .await?
            .check()?;
        // `RETURN` is the last statement before `COMMIT`; its slot follows the
        // statement count, as in `Exam::delete`.
        let slot = result.num_statements().saturating_sub(2);
        let removed = result.take::<Vec<ExamResult>>(slot)?;
        Ok(removed.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;

    #[tokio::test]
    async fn mark_range_is_enforced() {
        assert_eq!(Mark::try_new(0).unwrap().as_i64(), 0);
        assert_eq!(Mark::try_new(100).unwrap().as_i64(), 100);
        assert!(Mark::try_new(-1).is_err());
        assert!(Mark::try_new(101).is_err());
    }

    #[tokio::test]
    async fn retakes_keep_a_mark_per_sitting_with_the_latest_as_grade_of_record() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMAAAAAAAAAAAAAAAA");
        let user = UserId::from_key("01TESTSTUDENTAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");

        // Grade sitting #1, then a retake as sitting #2 — two rows, not one.
        ExamResult::grade(
            &exam,
            &user,
            1,
            Mark::try_new(40).unwrap(),
            &teacher,
            "midterm",
            &db,
        )
        .await
        .unwrap();
        let second = ExamResult::grade(
            &exam,
            &user,
            2,
            Mark::try_new(90).unwrap(),
            &teacher,
            "midterm",
            &db,
        )
        .await
        .unwrap();
        assert_eq!(second.get_seq(), 2);

        // Grade-of-record is the latest sitting's mark.
        let latest = ExamResult::read_for_user(&exam, &user, &db)
            .await
            .unwrap()
            .expect("graded");
        assert_eq!(latest.get_mark().as_i64(), 90);
        assert_eq!(latest.get_seq(), 2);

        // The roster yields exactly one row per pair — the latest.
        let roster = ExamResult::list_for_exam(&exam, &db).await.unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].get_mark().as_i64(), 90);

        // History keeps both sittings, oldest first.
        let history = ExamResult::list_all_for_exam_user(&exam, &user, &db)
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .map(|r| r.get_mark().as_i64())
                .collect::<Vec<_>>(),
            vec![40, 90]
        );

        // Deleting the pair removes every sitting.
        ExamResult::remove(&exam, &user, "midterm", &db)
            .await
            .unwrap();
        assert!(
            ExamResult::list_all_for_exam_user(&exam, &user, &db)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
