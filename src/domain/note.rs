use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN, NOTE_TABLE};
use crate::database::Database;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct NoteId(RecordId);

impl NoteId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// notes list `id DESC` (newest first, [`Note::list_for_user`]),
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

    pub async fn list_for(
        owner: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Note>, i64), AppError> {
        PagedList::new("note WHERE user = $usr", "ORDER BY id DESC")
            .bind("usr", owner.record())
            .run(limit, offset, db)
            .await
    }

    /// Request-scoped: no lock spans the handler's read and this write, so an
    /// omitted field (`None`) is not written at all. Passing the snapshot's
    /// value back instead would revert a concurrent edit of that field —
    /// scoping the `SET` alone does not prevent that, the values have to come
    /// from the request. Neither column is nullable, so plain `Option` per
    /// field says everything there is to say.
    pub async fn update(
        self,
        title: Option<NoteTitle>,
        content: Option<NoteContent>,
        db: &Database,
    ) -> Result<Note, AppError> {
        FieldUpdate::new(self.id.record())
            .set("title", title)
            .set("content", content)
            .run::<Note>(db)
            .await
    }

    /// Delete the note and cascade-remove its attachment rows. Blob files on
    /// disk are the web layer's to remove (it lists them before calling this);
    /// a crash in between leaves at worst an unreachable blob, never a row
    /// pointing at nothing.
    ///
    /// Children first, in one transaction, the way
    /// [`crate::domain::course::Course::delete`] does it: as two queries, an
    /// upload that committed in between kept its row while the note went, and
    /// nothing could ever list or delete it again.
    ///
    /// ponytail: the blob of *that* upload is still orphaned on disk — the web
    /// layer snapshots the file list before this call, so a row created inside
    /// the window is deleted here but its file is never unlinked. Ceiling: one
    /// leaked blob per racing upload-vs-delete, unreachable but not reclaimed.
    /// Upgrade path: return the deleted `note_file` rows from this transaction
    /// and have the handler unlink *those* instead of its snapshot.
    pub async fn delete(self, db: &Database) -> Result<Note, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE note_file WHERE note = $note;
                 LET $gone = (DELETE $note RETURN BEFORE);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("note", self.id.record()))
            .await?
            .check()?;
        result
            .take::<Vec<Note>>(3)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
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
