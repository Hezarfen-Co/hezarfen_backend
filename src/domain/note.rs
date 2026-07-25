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

    /// Field-scoped, mirroring [`crate::domain::subject::Subject::update`]: no
    /// lock spans the handler's read and this write, so only the two columns
    /// the request actually carries are written.
    pub async fn update(
        self,
        title: NoteTitle,
        content: NoteContent,
        db: &Database,
    ) -> Result<Note, AppError> {
        let mut result = db
            .query("UPDATE $id SET title = $title, content = $content RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("title", title))
            .bind(("content", content))
            .await?
            .check()?;
        result.take::<Vec<Note>>(0)?.into_iter().next().ok_or(AppError::NotFound)
    }

    /// Delete the note and cascade-remove its attachment rows. Blob files on
    /// disk are the web layer's to remove (it lists them before calling this);
    /// a crash in between leaves at worst an unreachable blob, never a row
    /// pointing at nothing.
    pub async fn delete(self, db: &Database) -> Result<Note, AppError> {
        db.query("DELETE note_file WHERE note = $note")
            .bind(("note", self.id.record()))
            .await?
            .check()?;
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
