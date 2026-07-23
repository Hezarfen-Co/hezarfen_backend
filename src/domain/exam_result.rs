use std::collections::HashMap;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::database::{Database, EXAM_RESULT_TABLE};
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_mark;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamResultId(RecordId);

impl ExamResultId {
    /// A deterministic id for the (exam, user, seq) triple — one mark row per
    /// sitting. The same triple always maps to the same record id, so grading a
    /// sitting is a single atomic UPSERT with no find-then-insert race. The
    /// first sitting keeps the historical `{exam}_{user}` shape (marks written
    /// before per-attempt history existed stay addressable unchanged); later
    /// sittings append their number. ULID keys are alphanumeric, so `_` is an
    /// unambiguous joiner.
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        let key = if seq == 1 {
            format!("{}_{}", exam.key(), user.key())
        } else {
            format!("{}_{}_{}", exam.key(), user.key(), seq)
        };
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
    pub async fn grade(
        exam: &ExamId,
        user: &UserId,
        seq: i64,
        mark: Mark,
        graded_by: &UserId,
        db: &Database,
    ) -> Result<ExamResult, AppError> {
        let result = ExamResult {
            id: ExamResultId::composite(exam, user, seq),
            exam: exam.clone(),
            user: user.clone(),
            seq,
            mark,
            graded_by: graded_by.clone(),
        };
        let saved: Option<ExamResult> = db.upsert(result.id.record()).content(result).await?;
        saved.ok_or_else(|| AppError::Internal("failed to record exam result".into()))
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

    /// True iff any exam of `kind` has produced a mark — the settings guard
    /// against removing an exam kind that grades already depend on (weights are
    /// read live from settings, so dropping the kind would silently re-weight
    /// those marks).
    pub async fn any_for_kind(kind: &str, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE id FROM exam_result
                 WHERE exam IN (SELECT VALUE id FROM exam WHERE kind = $kind) LIMIT 1",
            )
            .bind(("kind", kind.to_string()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
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

    pub async fn remove(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
        let mut result = db
            .query("DELETE exam_result WHERE exam = $ex AND user = $usr RETURN BEFORE")
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamResult>>(0)?.into_iter().next())
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
        ExamResult::grade(&exam, &user, 1, Mark::try_new(40).unwrap(), &teacher, &db)
            .await
            .unwrap();
        let second = ExamResult::grade(&exam, &user, 2, Mark::try_new(90).unwrap(), &teacher, &db)
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
            history.iter().map(|r| r.get_mark().as_i64()).collect::<Vec<_>>(),
            vec![40, 90]
        );

        // Deleting the pair removes every sitting.
        ExamResult::remove(&exam, &user, &db).await.unwrap();
        assert!(ExamResult::list_all_for_exam_user(&exam, &user, &db)
            .await
            .unwrap()
            .is_empty());
    }
}
