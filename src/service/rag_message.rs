//! RAG message workflows: the turns of a thread. Sending is asynchronous by
//! design — the send path writes the user's question plus a `pending`
//! assistant row and answers at once; the answering task settles that row
//! later with [`complete`] or [`fail`], and the read paths present a
//! long-stale `pending` as failed via the domain's read-time projection
//! (see [`RagMessage::projected`]). The queries live in
//! [`crate::db::rag_message`].

use crate::database::Database;
use crate::db::rag_message;
use crate::domain::rag_message::{RagContent, RagMessage, RagMessageId, RagReply};
use crate::domain::rag_thread::RagThreadId;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::types::Json;

/// Append the user's question. Nothing is awaited for it, so it is born
/// complete.
pub async fn append_user(
    db: &Database,
    thread: &RagThreadId,
    user: &UserId,
    content: RagContent,
) -> Result<RagMessage, AppError> {
    rag_message::append_user(db, thread, user, content).await
}

/// Reserve the assistant's answer *before* the AI call: the row exists,
/// empty and `pending`, so a reload finds the turn and can wait on it.
pub async fn append_pending_assistant(
    db: &Database,
    thread: &RagThreadId,
    user: &UserId,
) -> Result<RagMessage, AppError> {
    rag_message::append_pending_assistant(db, thread, user).await
}

/// The whole thread, oldest first — the order both the UI and the AI
/// history payload read in.
pub async fn list_for_thread(
    db: &Database,
    thread: &RagThreadId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagMessage>, i64), AppError> {
    rag_message::list_for_thread(db, thread, limit, offset).await
}

/// The last `limit` *settled* turns that carry text, still oldest-first —
/// the tail replayed to the AI service as context.
pub async fn list_settled_tail(
    db: &Database,
    thread: &RagThreadId,
    limit: usize,
) -> Result<Vec<RagMessage>, AppError> {
    rag_message::list_settled_tail(db, thread, limit).await
}

/// The user turn a reserved answer belongs to, by *identity*: the question
/// row of the very POST that reserved it, whose id the answering task
/// carries.
pub async fn prompt_of(
    db: &Database,
    id: &RagMessageId,
) -> Result<Option<RagMessage>, AppError> {
    rag_message::prompt_of(db, id).await
}

/// Read one turn only if `user` owns it — the poll loop's read.
pub async fn read_for(
    db: &Database,
    id: &RagMessageId,
    user: &UserId,
) -> Result<Option<RagMessage>, AppError> {
    rag_message::read_for(db, id, user).await
}

/// Land the answer: its text and the service's reply (abstention, reason,
/// citations). Gated on `status = 'pending'`, so a late answer can't
/// overwrite a row that was already failed.
pub async fn complete(
    db: &Database,
    id: &RagMessageId,
    text: RagContent,
    reply: Json<RagReply>,
) -> Result<RagMessage, AppError> {
    rag_message::complete(db, id, text, reply).await
}

/// Mark the answer failed with a short code (`unavailable`, the service's
/// own error code, …). Same pending gate as [`complete`].
pub async fn fail(
    db: &Database,
    id: &RagMessageId,
    error_code: &str,
) -> Result<RagMessage, AppError> {
    rag_message::fail(db, id, error_code).await
}
