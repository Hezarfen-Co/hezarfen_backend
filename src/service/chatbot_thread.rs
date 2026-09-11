//! Chatbot thread workflows: the create, rename and delete paths the
//! chatbot surface drives, all scoped to the owning user — a foreign id
//! reads as absent. The queries live in [`crate::db::chatbot_thread`].

use crate::database::Database;
use crate::db::chatbot_thread;
use crate::domain::chatbot_thread::{ChatbotThread, ChatbotThreadId, ChatbotThreadTitle};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Start a thread unless `user` is already at the school's
/// `max_chatbot_threads` (409 at the cap — the cap is storage protection,
/// not a usage quota). The cap check and the row write are one atomic
/// conditional write, so two requests racing the same user's last slot
/// cannot both win.
pub async fn create_capped(
    db: &Database,
    user: &UserId,
    title: Option<ChatbotThreadTitle>,
) -> Result<ChatbotThread, AppError> {
    chatbot_thread::create_capped(db, user, title).await
}

/// The caller's own threads, most recently active first.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ChatbotThread>, i64), AppError> {
    chatbot_thread::list_for_user(db, user, limit, offset).await
}

/// Read a thread only if `user` owns it — a foreign id reads as absent, so
/// the web layer answers 404 rather than leaking that it exists.
pub async fn read_for(
    db: &Database,
    id: &ChatbotThreadId,
    user: &UserId,
) -> Result<Option<ChatbotThread>, AppError> {
    chatbot_thread::read_for(db, id, user).await
}

/// Rename the thread (`None` clears the name back to untitled), stamping
/// the edit as activity so the thread moves to the top of the list.
pub async fn rename(
    db: &Database,
    thread: &ChatbotThread,
    title: Option<ChatbotThreadTitle>,
) -> Result<ChatbotThread, AppError> {
    chatbot_thread::rename(db, thread, title).await
}

/// Delete the thread and every turn in it — one transaction, so a crash
/// can't orphan messages under a vanished thread — and give the owner's
/// cap slot back in the same transaction.
pub async fn delete(db: &Database, thread: ChatbotThread) -> Result<ChatbotThread, AppError> {
    chatbot_thread::delete(db, thread).await
}
