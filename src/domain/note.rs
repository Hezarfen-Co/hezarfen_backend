use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN, NOTE_TABLE};
use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::NoteFile;
use crate::db::page::PagedList;
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

/// What [`Note::delete`]'s transaction removed: the note row (empty if it had
/// already vanished) and every attachment row the cascade took with it — the
/// only set whose blobs are safe to unlink.
#[derive(Debug, SurrealValue)]
struct DeleteOutcome {
    note: Vec<Note>,
    files: Vec<NoteFile>,
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

    /// Delete the note and cascade-remove its attachment rows, returning both:
    /// the note, and the attachment rows this transaction actually removed.
    /// Blob files on disk are the web layer's to remove, but only for *these*
    /// rows — a row uploaded after the caller listed the note's files is
    /// deleted here too, and a pre-read snapshot would strand its blob. A crash
    /// between commit and unlink leaves at worst an unreachable blob, never a
    /// row pointing at nothing.
    ///
    /// Children first, in one transaction, the way
    /// [`crate::domain::course::Course::delete`] does it: as two queries, an
    /// upload that committed in between kept its row while the note went, and
    /// nothing could ever list or delete it again.
    pub async fn delete(self, db: &Database) -> Result<(Note, Vec<NoteFile>), AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $files = (DELETE note_file WHERE note = $note RETURN BEFORE);
                 LET $gone = (DELETE $note RETURN BEFORE);
                 RETURN { note: $gone, files: $files };
                 COMMIT TRANSACTION;",
            )
            .bind(("note", self.id.record()))
            .await?
            .check()?;
        // BEGIN is slot 0, the two LETs slots 1-2; the RETURN is slot 3.
        let outcome = result
            .take::<Vec<DeleteOutcome>>(3)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to delete note".into()))?;
        let note = outcome.note.into_iter().next().ok_or(AppError::NotFound)?;
        Ok((note, outcome.files))
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

    /// The blobs the handler unlinks are exactly the rows this transaction
    /// removed — including one uploaded after any pre-read snapshot would have
    /// been taken, which is the row whose blob used to leak.
    #[tokio::test]
    async fn delete_returns_the_attachment_rows_it_removed() {
        use crate::domain::note_file::{FileContentType, FileName};

        let db = crate::database::init_mem().await.unwrap();
        let owner = crate::domain::user::UserId::generate();
        let note = Note::create(
            &owner,
            NoteTitle::try_new("a").unwrap(),
            NoteContent::try_new("body").unwrap(),
            &db,
        )
        .await
        .unwrap();
        // What a handler snapshot would have seen...
        let early = NoteFile::new(
            note.get_id(),
            FileName::try_new("early.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();
        let (snapshot, _) = NoteFile::list_for(note.get_id(), None, 0, &db)
            .await
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        // ...and the upload that races in after it.
        let late = NoteFile::new(
            note.get_id(),
            FileName::try_new("late.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();

        let (gone, files) = note.delete(&db).await.unwrap();
        assert_eq!(gone.get_title().as_str(), "a");
        let mut keys: Vec<_> = files
            .iter()
            .map(|file| file.get_id().key().to_string())
            .collect();
        keys.sort();
        let mut want = vec![
            early.get_id().key().to_string(),
            late.get_id().key().to_string(),
        ];
        want.sort();
        assert_eq!(keys, want);
        assert!(
            NoteFile::list_for(gone.get_id(), None, 0, &db)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[tokio::test]
    async fn content_is_optional() {
        assert_eq!(NoteContent::try_new("").unwrap().as_str(), "");
        assert_eq!(NoteContent::try_new("body").unwrap().as_str(), "body");
    }
}
