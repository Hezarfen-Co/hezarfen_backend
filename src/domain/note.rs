use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN, NOTE_TABLE};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct NoteId(RecordId);

impl NoteId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// notes list `id DESC` (newest first, [`crate::db::note::list_for`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(NOTE_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(NOTE_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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
#[derive(Debug, Clone, SurrealValue)]
pub struct Note {
    pub(crate) id: NoteId,
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
