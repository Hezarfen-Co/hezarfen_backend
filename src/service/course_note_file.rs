//! Course-note-file workflows: the attachment paths the course-notes surface
//! drives. The queries live in [`crate::db::course_note_file`]; blob
//! unlinking and the AI re-index stay in the web layer.

use crate::database::Database;
use crate::db::course_note_file;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::{CourseNoteFile, CourseNoteFileId};
use crate::error::AppError;

/// Persist a row assembled by [`CourseNoteFile::new`]; refuses with a
/// conflict once the note holds its cap.
pub async fn insert(db: &Database, file: CourseNoteFile) -> Result<CourseNoteFile, AppError> {
    course_note_file::insert(db, file).await
}

/// Read a file's row by id alone — the AI bridge reaches the note *through*
/// the file, so no note scoping happens here.
pub async fn read(
    db: &Database,
    id: &CourseNoteFileId,
) -> Result<Option<CourseNoteFile>, AppError> {
    course_note_file::read(db, id).await
}

/// Read a file's row only if it belongs to `note`.
pub async fn read_for(
    db: &Database,
    id: &CourseNoteFileId,
    note: &CourseNoteId,
) -> Result<Option<CourseNoteFile>, AppError> {
    course_note_file::read_for(db, id, note).await
}

pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseNoteFile>, i64), AppError> {
    course_note_file::list_for(db, note, limit, offset).await
}

/// Delete the row and hand its slot back in the same transaction; the
/// returned row is exactly whose blob the caller may unlink.
pub async fn delete(db: &Database, file: CourseNoteFile) -> Result<CourseNoteFile, AppError> {
    course_note_file::delete(db, file).await
}
