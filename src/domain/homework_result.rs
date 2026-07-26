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

use crate::constant::HOMEWORK_RESULT_TABLE;
use crate::database::Database;
use crate::domain::course::CourseId;
use crate::domain::exam_result::Mark;
use crate::domain::homework::HomeworkId;
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
    pub async fn grade(
        homework: &HomeworkId,
        user: &UserId,
        status: HomeworkStatus,
        mark: Option<Mark>,
        graded_by: &UserId,
        db: &Database,
    ) -> Result<HomeworkResult, AppError> {
        let result = HomeworkResult {
            id: HomeworkResultId::composite(homework, user),
            homework: homework.clone(),
            user: user.clone(),
            status,
            mark,
            graded_by: graded_by.clone(),
            created_at: Timestamp::now(),
        };
        let saved: Option<HomeworkResult> = db.upsert(result.id.record()).content(result).await?;
        saved.ok_or_else(|| AppError::Internal("failed to record homework result".into()))
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
    /// was none). Removing the grade unfreezes the student's submission.
    pub async fn remove(
        homework: &HomeworkId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<HomeworkResult>, AppError> {
        let mut result = db
            .query("DELETE homework_result WHERE homework = $hw AND user = $usr RETURN BEFORE")
            .bind(("hw", homework.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<HomeworkResult>>(0)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::HOMEWORK_STATUSES;
    use surrealdb::types::Value;

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
        let homework = HomeworkId::from_key("01TESTHWAAAAAAAAAAAAAAAAAA");
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
}
