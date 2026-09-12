//! A file attached to a course note. The row carries metadata only (original
//! filename, MIME type, byte size); the bytes themselves live on disk under
//! [`crate::config::Config::files_path`], in a file named by this row's key —
//! a server-generated ULID, so no user input ever shapes a disk path. The web
//! layer owns the blob I/O and its ordering (blob before row on upload, row
//! before blob on delete); persistence lives in
//! [`crate::db::course_note_file`]. `FileName` and `FileContentType` are
//! shared with [`crate::domain::note_file`] — same validation, no reason to
//! duplicate it.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

pub use crate::domain::note_file::{FileContentType, FileName};

use crate::constant::COURSE_NOTE_FILE_TABLE;
use crate::domain::course_note::CourseNoteId;
use crate::domain::monotonic_id::next_ulid;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseNoteFileId(RecordId);

impl CourseNoteFileId {
    /// Minted from the process-wide monotonic generator, not `Ulid::generate()`:
    /// a note's files list `id DESC` (newest first,
    /// [`crate::db::course_note_file::list_for`]), and a random low half
    /// scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(
            COURSE_NOTE_FILE_TABLE,
            next_ulid().to_string(),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(COURSE_NOTE_FILE_TABLE, key))
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

#[derive(Debug, Clone, SurrealValue)]
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
