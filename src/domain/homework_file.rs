//! A file attached to a homework submission. Like a note file, the row carries
//! metadata only (original filename, MIME type, byte size) and the bytes live
//! on disk under [`crate::config::Config::files_path`] — but named by this
//! row's own `file` field (a fresh server-generated UUID per upload, like a
//! question image), never by user input, so nothing a client sends shapes a
//! disk path. Files are immutable: created and deleted, never updated, so the
//! `submission`/`file`/`created_at` columns are only ever set once (READONLY
//! app discipline — single writer). The web layer owns the blob I/O and its
//! ordering (blob before row on upload, row before blob on delete); the rows
//! are written and read by [`crate::db::homework_file`], the upload/attach
//! workflow by [`crate::service::homework_file`].

use crate::domain::homework_submission::HomeworkSubmissionId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::note_file::{FileContentType, FileName};
use crate::domain::timestamp::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct HomeworkFileId(uuid::Uuid);

impl HomeworkFileId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: a submission's files list `id DESC` (newest first), and a random
    /// low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> uuid::Uuid {
        self.0
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

/// An attachment row. `file` is the blob's on-disk name (a fresh UUID per
/// upload), independent of the row id and reused as the GC key on cascade.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HomeworkFile {
    pub(crate) id: HomeworkFileId,
    pub(crate) submission: HomeworkSubmissionId,
    pub(crate) name: FileName,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
    pub(crate) file: String,
    pub(crate) created_at: Timestamp,
}

impl HomeworkFile {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`crate::db::homework_file::insert`] — so a stored row always points at
    /// a real blob.
    pub fn new(
        submission: &HomeworkSubmissionId,
        name: FileName,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: HomeworkFileId::generate(),
            submission: submission.clone(),
            name,
            content_type,
            size,
            file: uuid::Uuid::new_v4().to_string(),
            created_at: Timestamp::now(),
        }
    }

    pub fn get_id(&self) -> &HomeworkFileId {
        &self.id
    }

    pub fn get_submission(&self) -> &HomeworkSubmissionId {
        &self.submission
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

    /// The blob's on-disk name — a fresh UUID, so no user input shapes a path.
    pub fn get_file(&self) -> &str {
        &self.file
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }
}
