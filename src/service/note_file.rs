//! Note-file workflows: the attachment paths the notes surface drives. The
//! queries live in [`crate::db::note_file`]; blob unlinking stays in the web
//! layer.

use crate::database::Database;
use crate::db::note_file;
use crate::domain::note::NoteId;
use crate::domain::note_file::{NoteFile, NoteFileId};
use crate::error::AppError;

/// Persist a row assembled by [`NoteFile::new`]; refuses with a conflict once
/// the note holds its cap.
pub async fn insert(db: &Database, file: NoteFile) -> Result<NoteFile, AppError> {
    note_file::insert(db, file).await
}

/// Read a file's row only if it belongs to `note`.
pub async fn read_for(
    db: &Database,
    id: &NoteFileId,
    note: &NoteId,
) -> Result<Option<NoteFile>, AppError> {
    note_file::read_for(db, id, note).await
}

pub async fn list_for(
    db: &Database,
    note: &NoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<NoteFile>, i64), AppError> {
    note_file::list_for(db, note, limit, offset).await
}

/// Delete the row and hand its slot back in the same transaction; the returned
/// row is exactly whose blob the caller may unlink.
pub async fn delete(db: &Database, file: NoteFile) -> Result<NoteFile, AppError> {
    note_file::delete(db, file).await
}
