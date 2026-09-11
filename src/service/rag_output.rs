//! RAG-output workflows on the course-notes surface: list a note's outputs,
//! read and delete one, and cascade-drop every output of a note or of one
//! of its source files. The rows are derived data (the note is the source
//! of truth), so every path here is disposable by design. The queries live
//! in [`crate::db::rag_output`].

use crate::database::Database;
use crate::db::rag_output;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::rag_output::{RagOutput, RagOutputId};
use crate::error::AppError;

pub async fn read(db: &Database, id: &RagOutputId) -> Result<Option<RagOutput>, AppError> {
    rag_output::read(db, id).await
}

/// A note's outputs, newest first.
pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagOutput>, i64), AppError> {
    rag_output::list_for(db, note, limit, offset).await
}

pub async fn delete(db: &Database, id: &RagOutputId) -> Result<RagOutput, AppError> {
    rag_output::delete(db, id).await
}

/// Cascade: every output of `note`. Deleting none is a success — a note
/// no service ever indexed has nothing to drop.
pub async fn delete_for_note(db: &Database, note: &CourseNoteId) -> Result<(), AppError> {
    rag_output::delete_for_note(db, note).await
}

/// Cascade: every output built from `file`. Runs on a file delete even
/// with no AI service connected, so a stale output cannot survive its
/// source.
pub async fn delete_with_source(db: &Database, file: &CourseNoteFileId) -> Result<(), AppError> {
    rag_output::delete_with_source(db, file).await
}
