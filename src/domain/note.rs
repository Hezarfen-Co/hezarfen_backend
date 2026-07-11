use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN};
use crate::database::{Database, NOTE_TABLE};
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct NoteId(RecordId);

impl NoteId {
    pub fn generate() -> Self {
        Self(RecordId::new(NOTE_TABLE, Ulid::new().to_string()))
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

#[derive(Debug, Clone, SurrealValue)]
pub struct Note {
    id: NoteId,
    user: UserId,
    title: NoteTitle,
    content: NoteContent,
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

    pub async fn create(
        owner: &UserId,
        title: NoteTitle,
        content: NoteContent,
        db: &Database,
    ) -> Result<Note, AppError> {
        let note = Note {
            id: NoteId::generate(),
            user: owner.clone(),
            title,
            content,
        };
        let created: Option<Note> = db.create(note.id.record()).content(note).await?;
        created.ok_or_else(|| AppError::Internal("failed to create note".into()))
    }

    /// Read a note only if it belongs to `owner`.
    pub async fn read_owned(
        id: &NoteId,
        owner: &UserId,
        db: &Database,
    ) -> Result<Option<Note>, AppError> {
        let note: Option<Note> = db.select(id.record()).await?;
        Ok(note.filter(|note| &note.user == owner))
    }

    pub async fn list_for(owner: &UserId, db: &Database) -> Result<Vec<Note>, AppError> {
        let mut result = db
            .query("SELECT * FROM note WHERE user = $usr ORDER BY id DESC")
            .bind(("usr", owner.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Note>>(0)?)
    }

    pub async fn update(
        mut self,
        title: NoteTitle,
        content: NoteContent,
        db: &Database,
    ) -> Result<Note, AppError> {
        self.title = title;
        self.content = content;
        let updated: Option<Note> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    pub async fn delete(self, db: &Database) -> Result<Note, AppError> {
        let deleted: Option<Note> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
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
