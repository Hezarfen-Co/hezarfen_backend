//! One student's submission to a homework: an optional free-text note plus any
//! number of files ([`crate::domain::homework_file`]). The row id is the
//! deterministic `{homework}_{user}` composite, so a student has exactly one
//! submission per homework by construction and a re-submit is a single atomic
//! UPSERT.
//!
//! `submitted_at` — the first-submit stamp — is `READONLY` and survives every
//! re-submit; [`crate::db::homework_submission::upsert`] preserves it inside one
//! statement rather than through `.content()`, see there. `updated_at` moves to
//! now on every (re-)submit (and, later, on a file add/delete) and drives the
//! computed "late" flag (`updated_at > homework.due_at`, judged in the web
//! layer, never stored): `submitted_at` answers "was the first hand-in on
//! time", `updated_at` "was it touched after the deadline". The row writes
//! live in [`crate::db::homework_submission`], the submit/withdraw workflows
//! in [`crate::service::homework_submission`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{HOMEWORK_SUBMISSION_TABLE, MAX_HOMEWORK_TEXT_LEN};
use crate::domain::homework::HomeworkId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkSubmissionId(RecordId);

impl HomeworkSubmissionId {
    /// The one id a (homework, user) pair can have. A deterministic composite
    /// means one submission per pair with no unique index and no
    /// find-then-insert race — a re-submit UPSERTs the same row. ULID keys are
    /// alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(homework: &HomeworkId, user: &UserId) -> Self {
        Self(RecordId::new(
            HOMEWORK_SUBMISSION_TABLE,
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

/// A submission's optional free-text note: may be empty, at most
/// `MAX_HOMEWORK_TEXT_LEN` characters. Files ride separately.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SubmissionText(String);

impl SubmissionText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("text", value, MAX_HOMEWORK_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct HomeworkSubmission {
    pub(crate) id: HomeworkSubmissionId,
    pub(crate) homework: HomeworkId,
    pub(crate) user: UserId,
    pub(crate) text: Option<SubmissionText>,
    pub(crate) submitted_at: Timestamp,
    pub(crate) updated_at: Timestamp,
}

impl HomeworkSubmission {
    pub fn get_id(&self) -> &HomeworkSubmissionId {
        &self.id
    }

    pub fn get_homework(&self) -> &HomeworkId {
        &self.homework
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_text(&self) -> Option<&SubmissionText> {
        self.text.as_ref()
    }

    pub fn get_submitted_at(&self) -> Timestamp {
        self.submitted_at
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_is_optional_but_bounded() {
        assert!(SubmissionText::try_new("").is_ok());
        assert!(SubmissionText::try_new(&"x".repeat(5_000)).is_ok());
        assert!(SubmissionText::try_new(&"x".repeat(5_001)).is_err());
    }
}
