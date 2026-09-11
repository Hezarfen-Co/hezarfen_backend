//! Course-note workflows: the create/read/list/update/delete paths the
//! course-notes surface drives. The queries live in
//! [`crate::db::course_note`]; blob unlinking and the AI re-index stay in the
//! web layer.

use crate::database::Database;
use crate::db::course_note;
use crate::domain::course::CourseId;
use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteId, CourseNoteTitle};
use crate::domain::course_note_file::CourseNoteFile;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    course: &CourseId,
    author: &UserId,
    title: CourseNoteTitle,
    content: CourseNoteContent,
) -> Result<CourseNote, AppError> {
    course_note::create(db, course, author, title, content).await
}

pub async fn read(db: &Database, id: &CourseNoteId) -> Result<Option<CourseNote>, AppError> {
    course_note::read(db, id).await
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseNote>, i64), AppError> {
    course_note::list_for_course(db, course, limit, offset).await
}

/// Only what the request carried is written: an omitted field (`None`) is
/// not stored at all, so a concurrent edit of the other field survives.
pub async fn update(
    db: &Database,
    note: CourseNote,
    title: Option<CourseNoteTitle>,
    content: Option<CourseNoteContent>,
) -> Result<CourseNote, AppError> {
    course_note::update(db, note, title, content).await
}

/// Delete the note and cascade its attachment rows; the returned files are
/// exactly whose blobs the caller may unlink.
pub async fn delete(
    db: &Database,
    note: CourseNote,
) -> Result<(CourseNote, Vec<CourseNoteFile>), AppError> {
    course_note::delete(db, note).await
}
