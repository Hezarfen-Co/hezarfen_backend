//! A file attached to a note. The row carries metadata only (original
//! filename, MIME type, byte size); the bytes themselves live on disk under
//! [`crate::config::Config::files_path`], in a file named by this row's key —
//! a server-generated ULID, so no user input ever shapes a disk path. The web
//! layer owns the blob I/O and its ordering (blob before row on upload, row
//! before blob on delete); this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::constant::{MAX_FILE_CONTENT_TYPE_LEN, MAX_FILE_NAME_LEN, MAX_NOTE_FILES};
use crate::database::{Database, NOTE_FILE_TABLE};
use crate::domain::note::NoteId;
use crate::error::{AppError, ValidationError};

/// Serializes the files-per-note cap check against the insert (see
/// [`NoteFile::insert`]). This backend is the database's only
/// writer (single instance) — so one process-wide lock is sufficient.
// ponytail: global lock, per-note locks if uploads ever see real contention.
static FILE_CAP_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct NoteFileId(RecordId);

impl NoteFileId {
    pub fn generate() -> Self {
        Self(RecordId::new(NOTE_FILE_TABLE, Ulid::new().to_string()))
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
    /// already holds [`MAX_NOTE_FILES`]. The count-then-create runs under
    /// [`FILE_CAP_LOCK`] — a `BEGIN…COMMIT` can't enforce the cap because
    /// SurrealDB doesn't conflict-check a cross-record count against a
    /// concurrent insert (write-skew), the same story as
    /// `Registration::register`.
    pub async fn insert(self, db: &Database) -> Result<NoteFile, AppError> {
        let _guard = FILE_CAP_LOCK.lock().await;
        if Self::list_for(&self.note, db).await?.len() >= MAX_NOTE_FILES {
            return Err(AppError::Conflict(
                "the note already holds the maximum of 10 files — delete one first",
            ));
        }
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        let created: Option<NoteFile> = db.create(self.id.record()).content(self).await?;
        created.ok_or_else(|| AppError::Internal("failed to create note file".into()))
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
    pub async fn list_for(note: &NoteId, db: &Database) -> Result<Vec<NoteFile>, AppError> {
        let mut result = db
            .query("SELECT * FROM note_file WHERE note = $note ORDER BY id DESC")
            .bind(("note", note.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<NoteFile>>(0)?)
    }

    pub async fn delete(self, db: &Database) -> Result<NoteFile, AppError> {
        let deleted: Option<NoteFile> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
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
        let note_a = NoteId::generate();
        let note_b = NoteId::generate();
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

        assert_eq!(NoteFile::list_for(&note_a, &db).await.unwrap().len(), 1);
        assert!(NoteFile::list_for(&note_b, &db).await.unwrap().is_empty());

        file.delete(&db).await.unwrap();
        assert!(NoteFile::list_for(&note_a, &db).await.unwrap().is_empty());
    }
}
