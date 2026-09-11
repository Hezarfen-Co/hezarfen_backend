//! Note workflows: the create/read/list/update/delete paths the notes
//! surface drives, all scoped to the owning user. The queries live in
//! [`crate::db::note`]; blob unlinking stays in the web layer.

use crate::database::Database;
use crate::db::note;
use crate::domain::note::{Note, NoteContent, NoteId, NoteTitle};
use crate::domain::note_file::NoteFile;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    owner: &UserId,
    title: NoteTitle,
    content: NoteContent,
) -> Result<Note, AppError> {
    note::create(db, owner, title, content).await
}

/// Read a note only if it belongs to `owner`.
pub async fn read_owned(
    db: &Database,
    id: &NoteId,
    owner: &UserId,
) -> Result<Option<Note>, AppError> {
    note::read_owned(db, id, owner).await
}

pub async fn list_for(
    db: &Database,
    owner: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Note>, i64), AppError> {
    note::list_for(db, owner, limit, offset).await
}

/// Only what the request carried is written: an omitted field (`None`) is
/// not stored at all, so a concurrent edit of the other field survives.
pub async fn update(
    db: &Database,
    note: Note,
    title: Option<NoteTitle>,
    content: Option<NoteContent>,
) -> Result<Note, AppError> {
    note::update(db, note, title, content).await
}

/// Delete the note and cascade its attachment rows; the returned files are
/// exactly whose blobs the caller may unlink.
pub async fn delete(db: &Database, note: Note) -> Result<(Note, Vec<NoteFile>), AppError> {
    note::delete(db, note).await
}
