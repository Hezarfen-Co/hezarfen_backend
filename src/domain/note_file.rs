//! A file attached to a note. The row carries metadata only (original
//! filename, MIME type, byte size); the bytes themselves live on disk under
//! [`crate::config::Config::files_path`], in a file named by this row's key —
//! a server-generated ULID, so no user input ever shapes a disk path. The web
//! layer owns the blob I/O and its ordering (blob before row on upload, row
//! before blob on delete); this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    MAX_FILE_CONTENT_TYPE_LEN, MAX_FILE_NAME_LEN, MAX_NOTE_FILES, NOTE_FILE_COUNT_FIELD,
    NOTE_FILE_TABLE,
};
use crate::database::Database;
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note::NoteId;
use crate::error::{AppError, ValidationError};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct NoteFileId(RecordId);

impl NoteFileId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a note's files list `id DESC` (newest first, [`NoteFile::list_for_note`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(NOTE_FILE_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(NOTE_FILE_TABLE, key))
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

/// The uploader's original filename, kept for display and download headers.
/// Never used as a disk path — but path separators and control characters are
/// refused anyway: they are not filenames, only header/log injection attempts.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct FileName(String);

impl FileName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ValidationError::Empty("filename"));
        }
        if value.chars().count() > MAX_FILE_NAME_LEN {
            return Err(ValidationError::TooLong {
                field: "filename",
                max: MAX_FILE_NAME_LEN,
                got: value.chars().count(),
            });
        }
        if value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            return Err(ValidationError::Invalid {
                field: "filename",
                reason: "must not contain path separators or control characters",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The MIME type the client declared for the file. Blank defaults to
/// `application/octet-stream`; it is echoed back on download, so it is held
/// to header-safe ASCII.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct FileContentType(String);

impl FileContentType {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self("application/octet-stream".to_string()));
        }
        if value.chars().count() > MAX_FILE_CONTENT_TYPE_LEN {
            return Err(ValidationError::TooLong {
                field: "content_type",
                max: MAX_FILE_CONTENT_TYPE_LEN,
                got: value.chars().count(),
            });
        }
        if !value.is_ascii() {
            return Err(ValidationError::NotAscii("content_type"));
        }
        if value.chars().any(|c| c.is_ascii_control()) {
            return Err(ValidationError::Invalid {
                field: "content_type",
                reason: "must not contain control characters",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct NoteFile {
    id: NoteFileId,
    note: NoteId,
    name: FileName,
    content_type: FileContentType,
    size: i64,
}

impl NoteFile {
    /// Assemble a new attachment row (id generated here) without persisting
    /// it. The caller writes the blob to disk under the fresh id first, then
    /// calls [`Self::insert`] — so a stored row always points at a real blob.
    pub fn new(note: &NoteId, name: FileName, content_type: FileContentType, size: i64) -> Self {
        Self {
            id: NoteFileId::generate(),
            note: note.clone(),
            name,
            content_type,
            size,
        }
    }

    pub fn get_id(&self) -> &NoteFileId {
        &self.id
    }

    pub fn get_name(&self) -> &FileName {
        &self.name
    }

    pub fn get_content_type(&self) -> &FileContentType {
        &self.content_type
    }

    pub fn get_size(&self) -> i64 {
        self.size
    }

    /// Persist the row assembled by [`Self::new`], refusing once its note
    /// already holds [`MAX_NOTE_FILES`]. The slot and the row are taken together
    /// by [`cap::claim_and_create`] on the note row: a `BEGIN…COMMIT` around a
    /// count can't enforce the cap (SurrealDB doesn't conflict-check a
    /// cross-record count against a concurrent insert) and a process-wide mutex
    /// can't either, since it is released around the very round trip the insert
    /// races — a conditional single-record write can. Claiming in a *separate*
    /// query would enforce the cap but leak a slot on a crash between the two.
    pub async fn insert(self, db: &Database) -> Result<NoteFile, AppError> {
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        match cap::claim_and_create(
            &self.note.record(),
            NOTE_FILE_COUNT_FIELD,
            MAX_NOTE_FILES as i64,
            &self.id.record(),
            &self,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => Ok(created),
            // Also how a missing note reads: no note row means no slot to take.
            cap::Claimed::Full => Err(AppError::Conflict(
                "the note already holds the maximum of 10 files — delete one first",
            )),
            // Unreachable: the id is a ULID this call just generated.
            cap::Claimed::Duplicate => Err(AppError::Internal("failed to create note file".into())),
        }
    }

    /// Read a file's row only if it belongs to `note` — callers have already
    /// checked the note belongs to the requesting user.
    pub async fn read_for(
        id: &NoteFileId,
        note: &NoteId,
        db: &Database,
    ) -> Result<Option<NoteFile>, AppError> {
        let file: Option<NoteFile> = db.select(id.record()).await?;
        Ok(file.filter(|file| &file.note == note))
    }

    /// All of `note`'s attachment rows, newest first.
    pub async fn list_for(
        note: &NoteId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<NoteFile>, i64), AppError> {
        PagedList::new("note_file WHERE note = $note", "ORDER BY id DESC")
            .bind("note", note.record())
            .run(limit, offset, db)
            .await
    }

    /// Delete the row and give its slot back in the same transaction — the note
    /// itself is untouched, so unlike the note-delete cascade this one has a
    /// counter to correct. (Deleting a *note* takes its counter with it.)
    pub async fn delete(self, db: &Database) -> Result<NoteFile, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE $id RETURN BEFORE);
                 UPDATE $note SET file_count = math::max([(file_count ?? 0) - array::len($gone), 0]);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("id", self.id.record()))
            .bind(("note", self.note.record()))
            .await?
            .check()?;
        result
            .take::<Vec<NoteFile>>(3)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn filename_rules() {
        assert_eq!(
            FileName::try_new(" ödev.pdf ").unwrap().as_str(),
            "ödev.pdf"
        );
        assert!(FileName::try_new("").is_err());
        assert!(FileName::try_new("   ").is_err());
        assert!(FileName::try_new(&"x".repeat(256)).is_err());
        // Path separators and control characters are injection, not filenames.
        assert!(FileName::try_new("a/b.pdf").is_err());
        assert!(FileName::try_new("a\\b.pdf").is_err());
        assert!(FileName::try_new("a\nb.pdf").is_err());
        assert!(FileName::try_new("a\x00b").is_err());
    }

    #[tokio::test]
    async fn content_type_rules() {
        assert_eq!(
            FileContentType::try_new("application/pdf")
                .unwrap()
                .as_str(),
            "application/pdf"
        );
        // Blank means the client didn't say — fall back, don't fail.
        assert_eq!(
            FileContentType::try_new("").unwrap().as_str(),
            "application/octet-stream"
        );
        assert!(FileContentType::try_new(&"x".repeat(101)).is_err());
        assert!(FileContentType::try_new("appli¢ation/pdf").is_err());
        assert!(FileContentType::try_new("a\r\nb").is_err());
    }

    #[tokio::test]
    async fn rows_scope_to_their_note() {
        let db = crate::database::init_mem().await.unwrap();
        // Real note rows: the cap counter lives on the note, so an insert whose
        // note does not exist has no slot to take (a 409, like a full note).
        let owner = crate::domain::user::UserId::generate();
        let note_of = async |title: &str| {
            crate::db::note::create(
                &db,
                &owner,
                crate::domain::note::NoteTitle::try_new(title).unwrap(),
                crate::domain::note::NoteContent::try_new("body").unwrap(),
            )
            .await
            .unwrap()
        };
        let note_a = note_of("a").await.get_id().clone();
        let note_b = note_of("b").await.get_id().clone();
        let file = NoteFile::new(
            &note_a,
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();

        // Readable under its own note, invisible under another.
        let found = NoteFile::read_for(file.get_id(), &note_a, &db)
            .await
            .unwrap();
        assert_eq!(found.unwrap().get_name().as_str(), "plan.pdf");
        assert!(
            NoteFile::read_for(file.get_id(), &note_b, &db)
                .await
                .unwrap()
                .is_none()
        );

        let listed = async |note: &NoteId| NoteFile::list_for(note, None, 0, &db).await.unwrap().0;
        assert_eq!(listed(&note_a).await.len(), 1);
        assert!(listed(&note_b).await.is_empty());

        file.delete(&db).await.unwrap();
        assert!(listed(&note_a).await.is_empty());
    }

    /// The counter and the rows are written in one transaction, so the stored
    /// count must equal the stored rows — after a success *and* after the
    /// refusal that fills the cap, which must move neither.
    #[tokio::test]
    async fn counter_tracks_stored_rows() {
        let db = crate::database::init_mem().await.unwrap();
        let note = crate::db::note::create(
            &db,
            &crate::domain::user::UserId::generate(),
            crate::domain::note::NoteTitle::try_new("a").unwrap(),
            crate::domain::note::NoteContent::try_new("body").unwrap(),
        )
        .await
        .unwrap()
        .get_id()
        .clone();
        let stored_count = async |note: &NoteId| -> i64 {
            db.query("SELECT VALUE file_count FROM $note")
                .bind(("note", note.record()))
                .await
                .unwrap()
                .take::<Vec<i64>>(0)
                .unwrap()
                .into_iter()
                .next()
                .unwrap_or(0)
        };
        let add = async |note: &NoteId| {
            NoteFile::new(
                note,
                FileName::try_new("plan.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            )
            .insert(&db)
            .await
        };

        for filled in 1..=MAX_NOTE_FILES {
            add(&note).await.unwrap();
            assert_eq!(stored_count(&note).await, filled as i64);
            assert_eq!(
                NoteFile::list_for(&note, None, 0, &db).await.unwrap().1,
                filled as i64
            );
        }

        // At the cap: the refusal writes nothing, counter included.
        assert!(matches!(add(&note).await, Err(AppError::Conflict(_))));
        assert_eq!(stored_count(&note).await, MAX_NOTE_FILES as i64);
        assert_eq!(
            NoteFile::list_for(&note, None, 0, &db).await.unwrap().1,
            MAX_NOTE_FILES as i64
        );
    }
}
