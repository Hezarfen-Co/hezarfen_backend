//! Pool-question funnels: ask, the two listings, the approve with its
//! 409-vs-404 split, the pending-only image writes, and the delete whose
//! swept solutions and collected image blob keys ride back for the web
//! layer's blob cleanup. The queries and their transactions live in
//! [`crate::db::pool_question`]; the pure entity and newtypes in
//! [`crate::domain::pool_question`].

use crate::database::Database;
use crate::db::pool_question;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::{PoolQuestion, PoolQuestionId};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn insert(db: &Database, question: PoolQuestion) -> Result<PoolQuestion, AppError> {
    pool_question::insert(db, question).await
}

pub async fn read(db: &Database, id: &PoolQuestionId) -> Result<Option<PoolQuestion>, AppError> {
    pool_question::read(db, id).await
}

/// Every question, newest first — the teacher+ view (approval queue and
/// pool in one list).
pub async fn list_all(db: &Database) -> Result<Vec<PoolQuestion>, AppError> {
    pool_question::list_all(db).await
}

/// The pool as a non-staff user sees it, newest first: every approved
/// question, plus the caller's own pending ones.
pub async fn list_visible_to(db: &Database, user: &UserId) -> Result<Vec<PoolQuestion>, AppError> {
    pool_question::list_visible_to(db, user).await
}

/// Approve a pending question, stamping `approver`. A `None` from the
/// guarded write is ambiguous — already approved, or gone — so this sorts it
/// out with a read: the question missing is a 404, anything else is the 409
/// the route's second approve answers.
pub async fn approve(
    db: &Database,
    id: &PoolQuestionId,
    approver: &UserId,
) -> Result<PoolQuestion, AppError> {
    match pool_question::approve(db, id, approver).await? {
        Some(question) => Ok(question),
        // Nothing was pending under that id: either it's already approved
        // (409) or it never existed / was deleted (404).
        None => {
            if pool_question::read(db, id).await?.is_none() {
                return Err(AppError::NotFound);
            }
            Err(AppError::Conflict("the question is already approved"))
        }
    }
}

/// Point the question at a freshly written image blob; `None` means the
/// question was approved or deleted mid-upload (the fresh blob is the
/// caller's orphan to take back off disk). `Some(replaced)` names the blob
/// this upload displaced — the caller's to remove.
pub async fn set_image(
    db: &Database,
    id: &PoolQuestionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<Option<String>>, AppError> {
    pool_question::set_image(db, id, file, content_type, size).await
}

/// Detach the question's image (pending only, like [`set_image`]); `None`
/// means the question is no longer pending. `Some(detached)` names the
/// removed blob — `None` inside when there was no image.
pub async fn clear_image(
    db: &Database,
    id: &PoolQuestionId,
) -> Result<Option<Option<String>>, AppError> {
    pool_question::clear_image(db, id).await
}

/// Delete the question and cascade its solutions; the removed rows and the
/// image blob keys they named come back so the web layer can take every
/// blob off disk.
pub async fn delete(
    db: &Database,
    id: &PoolQuestionId,
) -> Result<Option<pool_question::Deleted>, AppError> {
    pool_question::delete(db, id).await
}
