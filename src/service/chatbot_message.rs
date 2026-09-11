//! Chatbot message workflows: the turns of a thread. Sending is
//! asynchronous by design — the send path writes the user's prompt plus a
//! `pending` assistant row and answers at once; the answering task settles
//! that row later with [`complete`] or [`fail`], and the read paths present
//! a long-stale `pending` as failed via the domain's read-time projection
//! (see [`ChatbotMessage::projected`]). The queries live in
//! [`crate::db::chatbot_message`].

use crate::database::Database;
use crate::db::chatbot_message;
use crate::domain::chatbot_message::{ChatContent, ChatbotMessage, ChatbotMessageId};
use crate::domain::chatbot_thread::ChatbotThreadId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Append the user's prompt. Nothing is awaited for it, so it is born
/// complete.
pub async fn append_user(
    db: &Database,
    thread: &ChatbotThreadId,
    user: &UserId,
    content: ChatContent,
) -> Result<ChatbotMessage, AppError> {
    chatbot_message::append_user(db, thread, user, content).await
}

/// Reserve the assistant's answer *before* the AI call: the row exists,
/// empty and `pending`, so a reload finds the turn and can wait on it.
pub async fn append_pending_assistant(
    db: &Database,
    thread: &ChatbotThreadId,
    user: &UserId,
) -> Result<ChatbotMessage, AppError> {
    chatbot_message::append_pending_assistant(db, thread, user).await
}

/// The whole thread, oldest first — the order both the UI and the AI
/// history payload read in.
pub async fn list_for_thread(
    db: &Database,
    thread: &ChatbotThreadId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ChatbotMessage>, i64), AppError> {
    chatbot_message::list_for_thread(db, thread, limit, offset).await
}

/// The last `limit` *settled* turns that carry text, still oldest-first —
/// the tail replayed to the AI service as context.
pub async fn list_settled_tail(
    db: &Database,
    thread: &ChatbotThreadId,
    limit: usize,
) -> Result<Vec<ChatbotMessage>, AppError> {
    chatbot_message::list_settled_tail(db, thread, limit).await
}

/// The user turn a reserved answer belongs to, by *identity*: the prompt
/// row of the very POST that reserved it, whose id the answering task
/// carries.
pub async fn prompt_of(
    db: &Database,
    id: &ChatbotMessageId,
) -> Result<Option<ChatbotMessage>, AppError> {
    chatbot_message::prompt_of(db, id).await
}

/// Read one turn only if `user` owns it — the poll loop's read.
pub async fn read_for(
    db: &Database,
    id: &ChatbotMessageId,
    user: &UserId,
) -> Result<Option<ChatbotMessage>, AppError> {
    chatbot_message::read_for(db, id, user).await
}

/// Land the answer, recording whether it had to be clipped to fit the
/// school's cap. Gated on `status = 'pending'`, so a late reply can't
/// overwrite a row that was already failed.
pub async fn complete(
    db: &Database,
    id: &ChatbotMessageId,
    text: ChatContent,
    truncated: bool,
) -> Result<ChatbotMessage, AppError> {
    chatbot_message::complete(db, id, text, truncated).await
}

/// Mark the answer failed with a short code (`unavailable`, the service's
/// own error code, …). Same pending gate as [`complete`].
pub async fn fail(
    db: &Database,
    id: &ChatbotMessageId,
    error_code: &str,
) -> Result<ChatbotMessage, AppError> {
    chatbot_message::fail(db, id, error_code).await
}
