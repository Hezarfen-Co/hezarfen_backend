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
    /// A deterministic id for the (exam, user) pair. The same pair always maps to
    /// the same record id, so grading is a single atomic UPSERT with no
    /// find-then-insert race and one-row-per-pair by construction. ULID keys are
    /// alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(exam: &ExamId, user: &UserId) -> Self {
        Self(RecordId::new(
            EXAM_RESULT_TABLE,
            format!("{}_{}", exam.key(), user.key()),
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
        let mut result = db
            .query("SELECT * FROM exam_result WHERE exam = $ex AND user = $usr LIMIT 1")
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamResult>>(0)?.into_iter().next())
    }

    /// A single user's result for an exam, if graded.
    pub async fn read_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
        Self::find(exam, user, db).await
    }

    /// Record (or overwrite) `user`'s mark for `exam`. One row per (exam, user),
    /// keyed by a deterministic composite id so this is a single atomic UPSERT —
    /// concurrent grades for the same pair converge on one row instead of racing
    /// the unique index into a 500.
    pub async fn grade(
        exam: &ExamId,
        user: &UserId,
        mark: Mark,
        graded_by: &UserId,
        db: &Database,
    ) -> Result<ExamResult, AppError> {
        let result = ExamResult {
            id: ExamResultId::composite(exam, user),
            exam: exam.clone(),
            user: user.clone(),
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
        Ok(result.take::<Vec<ExamResult>>(0)?)
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

    #[tokio::test]
    async fn mark_range_is_enforced() {
        assert_eq!(Mark::try_new(0).unwrap().as_i64(), 0);
        assert_eq!(Mark::try_new(100).unwrap().as_i64(), 100);
        assert!(Mark::try_new(-1).is_err());
        assert!(Mark::try_new(101).is_err());
    }
}
