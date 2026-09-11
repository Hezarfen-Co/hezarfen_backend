//! A teacher's grade on one student's homework: a status
//! (`done`/`incomplete`/`missing`) plus an optional numeric [`Mark`] (0..=100,
//! reused from exam results). Like an exam result, the row id is the
//! deterministic `{homework}_{user}` composite, so grading is one atomic UPSERT
//! and there is exactly one grade per (homework, user) by construction.
//!
//! A stored result is what *freezes* a submission: while a grade exists the
//! student's submission and files are locked (the web layer answers 409),
//! until the teacher removes the grade to reopen them. The teacher-set
//! `missing` status is a deliberate verdict, distinct from the roster's
//! *computed* "missing" (unsubmitted past due) — the latter is derived in the
//! web layer, never stored. The grade transaction lives in
//! [`crate::db::homework_result`], the grading gates in
//! [`crate::service::homework_result`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::HOMEWORK_RESULT_TABLE;
use crate::domain::exam_result::Mark;
use crate::domain::homework::HomeworkId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
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
}
