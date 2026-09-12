//! A file attached to a course note. The row carries metadata only (original
//! filename, MIME type, byte size); the bytes themselves live on disk under
//! [`crate::config::Config::files_path`], in a file named by this row's `file`
//! field — a server-generated UUID, so no user input ever shapes a disk path.
//! The web layer owns the blob I/O and its ordering (blob before row on
//! upload, row before blob on delete); persistence lives in
//! [`crate::db::course_note_file`]. `FileName` and `FileContentType` are
//! shared with [`crate::domain::note_file`] — same validation, no reason to
//! duplicate it.

pub use crate::domain::note_file::{FileContentType, FileName};

use crate::domain::course_note::CourseNoteId;
use crate::domain::monotonic_id::next_uuid;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseNoteFileId(uuid::Uuid);

impl CourseNoteFileId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: a note's files list `id DESC` (newest first,
    /// [`crate::db::course_note_file::list_for`]), and a random low half
    /// scrambles rows minted in the same millisecond.
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


#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CourseNoteFile {
    pub(crate) id: CourseNoteFileId,
    pub(crate) course_note: CourseNoteId,
    pub(crate) name: FileName,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl CourseNoteFile {
    /// Assemble a new attachment row (id generated here) without persisting
    /// it. The caller writes the blob to disk under the fresh id first, then
    /// calls [`crate::db::course_note_file::insert`] — so a stored row always
    /// points at a real blob.
    pub fn new(
        note: &CourseNoteId,
        name: FileName,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: CourseNoteFileId::generate(),
            course_note: note.clone(),
            name,
            content_type,
            size,
        }
    }

    pub fn get_id(&self) -> &CourseNoteFileId {
        &self.id
    }

    pub fn get_course_note(&self) -> &CourseNoteId {
        &self.course_note
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
}
