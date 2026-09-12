use uuid::Uuid;

use crate::constant::{MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

/// Typed note row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct NoteId(Uuid);

impl NoteId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// notes list `id DESC` (newest first, [`crate::db::note::list_for`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct NoteTitle(String);

impl NoteTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_NOTE_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct NoteContent(String);

impl NoteContent {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("content", value, MAX_NOTE_CONTENT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fields are crate-visible: [`crate::db::note`] mints the rows on create and
/// reads the id when updating and cascading a delete.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Note {
    pub(crate) id: NoteId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) title: NoteTitle,
    pub(crate) content: NoteContent,
}

impl Note {
    pub fn get_id(&self) -> &NoteId {
        &self.id
    }

    pub fn get_title(&self) -> &NoteTitle {
        &self.title
    }

    pub fn get_content(&self) -> &NoteContent {
        &self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert_eq!(NoteTitle::try_new("hi").unwrap().as_str(), "hi");
        assert!(NoteTitle::try_new("  ").is_err());
    }

    #[tokio::test]
    async fn content_is_optional() {
        assert_eq!(NoteContent::try_new("").unwrap().as_str(), "");
        assert_eq!(NoteContent::try_new("body").unwrap().as_str(), "body");
    }
}
