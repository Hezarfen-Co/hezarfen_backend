//! One student's submission to a homework: an optional free-text note plus any
//! number of files ([`crate::domain::homework_file`]). The row carries its own
//! `id` primary key and a `UNIQUE (homework, app_user)` constraint, so a
//! student has exactly one submission per homework by construction and a
//! re-submit is a single atomic UPSERT on the pair.
//!
//! `submitted_at` — the first-submit stamp — is write-once and survives every
//! re-submit; [`crate::db::homework_submission::upsert`] preserves it inside
//! one statement rather than overwriting the whole row. `updated_at` moves to
//! now on every (re-)submit (and, later, on a file add/delete) and drives the
//! computed "late" flag (`updated_at > homework.due_at`, judged in the web
//! layer, never stored): `submitted_at` answers "was the first hand-in on
//! time", `updated_at` "was it touched after the deadline". The row writes
//! live in [`crate::db::homework_submission`], the submit/withdraw workflows
//! in [`crate::service::homework_submission`].

use crate::constant::MAX_HOMEWORK_TEXT_LEN;
use crate::domain::homework::HomeworkId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HomeworkSubmissionId(uuid::Uuid);

impl HomeworkSubmissionId {
    /// A write-ordered id: submission listings read newest first.
    pub fn generate() -> Self {
        Self(next_uuid())
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

/// A submission's optional free-text note: may be empty, at most
/// `MAX_HOMEWORK_TEXT_LEN` characters. Files ride separately.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
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
