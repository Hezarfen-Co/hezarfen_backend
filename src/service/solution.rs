//! Solution funnels: the offer (through the question's existence-move), the
//! question-scoped read and page, the grouped counts, and the unconditional
//! body/image writes of an unmoderated row. The queries live in
//! [`crate::db::solution`]; the pure entity and newtypes in
//! [`crate::domain::solution`].

use std::collections::HashMap;

use crate::database::Database;
use crate::db::solution;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::solution::{Solution, SolutionBody, SolutionId};
use crate::error::AppError;

/// Offer the solution; `NotFound` = the question is gone, and nothing was
/// written.
pub async fn insert(db: &Database, offer: Solution) -> Result<Solution, AppError> {
    solution::insert(db, offer).await
}

/// Read a solution only if it belongs to `question` — keeps the nested
/// route honest (a solution id under someone else's question 404s).
pub async fn read_for(
    db: &Database,
    id: &SolutionId,
    question: &PoolQuestionId,
) -> Result<Option<Solution>, AppError> {
    solution::read_for(db, id, question).await
}

/// The question's solutions, oldest first.
pub async fn list_for(
    db: &Database,
    question: &PoolQuestionId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Solution>, i64), AppError> {
    solution::list_for(db, question, limit, offset).await
}

/// Per-question solution tallies for a page of questions, in one grouped
/// query.
pub async fn counts_for(
    db: &Database,
    questions: &[PoolQuestionId],
) -> Result<HashMap<String, i64>, AppError> {
    solution::counts_for(db, questions).await
}

/// Replace the body; `None` only means the row was deleted mid-flight.
pub async fn set_body(
    db: &Database,
    id: &SolutionId,
    body: &SolutionBody,
) -> Result<Option<Solution>, AppError> {
    solution::set_body(db, id, body).await
}

/// Point the solution at a freshly written image blob; the *before* row names
/// the replaced blob the caller must remove.
pub async fn set_image(
    db: &Database,
    id: &SolutionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<Solution>, AppError> {
    solution::set_image(db, id, file, content_type, size).await
}

/// Detach the solution's image; the *before* row names the blob the caller
/// must remove.
pub async fn clear_image(db: &Database, id: &SolutionId) -> Result<Option<Solution>, AppError> {
    solution::clear_image(db, id).await
}

pub async fn delete(db: &Database, target: Solution) -> Result<Solution, AppError> {
    solution::delete(db, target).await
}
