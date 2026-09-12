//! A teacher's grade on one student's homework: a status
//! (`done`/`incomplete`/`missing`) plus an optional numeric [`Mark`] (0..=100,
//! reused from exam results). The row carries its own `id` primary key and a
//! `UNIQUE (homework, app_user)` constraint, so grading is one atomic UPSERT
//! (`ON CONFLICT (homework, app_user)`) and there is exactly one grade per
//! (homework, user) by construction.
//!
//! A stored result is what *freezes* a submission: while a grade exists the
//! student's submission and files are locked (the web layer answers 409),
//! until the teacher removes the grade to reopen them. The teacher-set
//! `missing` status is a deliberate verdict, distinct from the roster's
//! *computed* "missing" (unsubmitted past due) — the latter is derived in the
//! web layer, never stored. The grade transaction lives in
//! [`crate::db::homework_result`], the grading gates in
//! [`crate::service::homework_result`].

use crate::domain::exam_result::Mark;
use crate::domain::homework::HomeworkId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_homework_status;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HomeworkResultId(uuid::Uuid);

impl HomeworkResultId {
    /// A write-ordered id: grade listings read newest first.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// A validated homework grade: `done` (submitted and complete), `incomplete`
/// (submitted but lacking), or `missing` (not done). Held to the
/// [`crate::constant::HOMEWORK_STATUSES`] set by [`HomeworkStatus::try_new`] —
/// the same validated-string-against-a-const shape as
/// [`crate::domain::exam::ExamMode`]. It stores as the bare string. A stored
/// `missing` is a teacher's deliberate verdict, distinct from the roster's
/// *computed* missing (unsubmitted past due, derived in the web layer, never
/// stored).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HomeworkResult {
    pub(crate) id: HomeworkResultId,
    pub(crate) homework: HomeworkId,
    pub(crate) user: UserId,
    pub(crate) status: HomeworkStatus,
    pub(crate) mark: Option<Mark>,
    pub(crate) graded_by: UserId,
    pub(crate) created_at: Timestamp,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::HOMEWORK_STATUSES;

    #[tokio::test]
    async fn status_is_held_to_the_const() {
        // Enforcement lives in the newtype against HOMEWORK_STATUSES, and the
        // stored value is the bare validated string the `status` TEXT column
        // accepts — queries match on these spellings, so they may not drift.
        for status in HOMEWORK_STATUSES {
            let parsed = HomeworkStatus::try_new(status).unwrap();
            assert_eq!(parsed.as_str(), status);
        }
        assert!(HomeworkStatus::try_new("late").is_err());
        assert!(HomeworkStatus::try_new("").is_err());
    }
}
