//! RAG thread workflows: the create, rename and delete paths the RAG
//! surface drives, all scoped to the owning user — a foreign id reads as
//! absent. The queries live in [`crate::db::rag_thread`].

use crate::database::Database;
use crate::db::rag_thread;
use crate::domain::rag_thread::{RagThread, RagThreadId, RagThreadTitle};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Start a thread unless `user` is already at the school's
/// `max_chatbot_threads` (409 at the cap — the cap is storage protection,
/// not a usage quota). The cap check and the row write are one atomic
/// conditional write, so two requests racing the same user's last slot
/// cannot both win. The knob is the chatbot nest's — one AI package, one
/// ceiling — while the seats are this nest's own.
pub async fn create_capped(
    db: &Database,
    user: &UserId,
    title: Option<RagThreadTitle>,
) -> Result<RagThread, AppError> {
    rag_thread::create_capped(db, user, title).await
}

/// The caller's own threads, most recently active first.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagThread>, i64), AppError> {
    rag_thread::list_for_user(db, user, limit, offset).await
}

/// Read a thread only if `user` owns it — a foreign id reads as absent, so
/// the web layer answers 404 rather than leaking that it exists.
pub async fn read_for(
    db: &Database,
    id: &RagThreadId,
    user: &UserId,
) -> Result<Option<RagThread>, AppError> {
    rag_thread::read_for(db, id, user).await
}

/// Rename the thread (`None` clears the name back to untitled), stamping
/// the edit as activity so the thread moves to the top of the list.
pub async fn rename(
    db: &Database,
    thread: &RagThread,
    title: Option<RagThreadTitle>,
) -> Result<RagThread, AppError> {
    rag_thread::rename(db, thread, title).await
}

/// Delete the thread and every turn in it — one transaction, so a crash
/// can't orphan messages under a vanished thread — and give the owner's
/// cap slot back in the same transaction.
pub async fn delete(db: &Database, thread: RagThread) -> Result<RagThread, AppError> {
    rag_thread::delete(db, thread).await
}
