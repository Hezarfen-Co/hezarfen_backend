//! The `chatbot_message` table: the turns of a thread, both sides persisted.
//! The user's prompt lands `complete`, and the assistant's row is written
//! `pending` *before* the AI call so a reload never loses an answer in
//! flight. The task that owns the bridge round trip then flips it to
//! `complete` (with the text) or `failed` (with a code), and the boot sweep
//! in `database.rs` fails whatever a process death left behind past the
//! staleness window. Every turn is written *through* its thread's own row —
//! its guarded statement bumps `updated_at`, so a turn can never outlive
//! the thread it belongs to. The row shape lives in
//! [`crate::domain::chatbot_message`].

use crate::constant::MAX_ERROR_CODE_LEN;
use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::chatbot_message::{
    ChatContent, ChatbotMessage, ChatbotMessageId, MessageRole, MessageStatus,
};
use crate::domain::chatbot_thread::ChatbotThreadId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

/// Write one turn *through its thread's own row*: the thread's activity
/// bump and the turn's insert are a single guarded statement, which is what
/// makes the thread's existence something this write writes rather than
/// something the caller read and then trusted — a
/// [`delete`](crate::db::chatbot_thread::delete) racing it touches the very
/// row this statement's gate updates, so under Postgres's row locking the
/// two serialize and no turn is left under a thread that is gone. It also
/// carries the thread's activity stamp, so a turn and its `updated_at` land
/// together. The stamp is written strictly upwards —
/// `GREATEST($now, updated_at + 1)` — because a turn's two rows routinely
/// land inside one millisecond and `updated_at` is the thread list's
/// ordering key.
///
/// [`AppError::NotFound`] = the thread is gone, and nothing was written.
async fn insert(db: &Database, message: ChatbotMessage) -> Result<ChatbotMessage, AppError> {
    let inserted = query_as!(
        ChatbotMessage,
        "WITH touch AS (
             UPDATE chatbot_thread SET updated_at = GREATEST($2, updated_at + 1)
             WHERE id = $1
             RETURNING 1)
         INSERT INTO chatbot_message (id, thread_id, user_id, role, content, status, \
                                      truncated, error_code, created_at, completed_at)
         SELECT $3, $1, $4, $5, $6, $7, $8, $9, $10, $11
         WHERE EXISTS (SELECT 1 FROM touch)
         RETURNING id AS \"id: ChatbotMessageId\", thread_id AS \"thread_id: ChatbotThreadId\", \
                   user_id AS \"user_id: UserId\", role AS \"role: MessageRole\", content AS \"content: ChatContent\", \
                   status AS \"status: MessageStatus\", truncated, \
                   error_code AS \"error_code: String\", \
                   created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\"",
        message.get_thread_id().uuid(),
        Timestamp::now().as_millis(),
        message.get_id().uuid(),
        message.user_id.uuid(),
        message.role.as_str(),
        message.content.as_str(),
        message.status.as_str(),
        message.truncated,
        message.error_code,
        message.created_at.as_millis(),
        message.completed_at.map(|t| t.as_millis()),
    )
    .fetch_optional(db)
    .await?;
    inserted.ok_or_else(|| AppError::Internal("failed to create chat message".into()))
}

/// Append the user's prompt. Nothing is awaited for it, so it is born
/// complete.
pub async fn append_user(
    db: &Database,
    thread: &ChatbotThreadId,
    user: &UserId,
    content: ChatContent,
) -> Result<ChatbotMessage, AppError> {
    let now = Timestamp::now();
    insert(
        db,
        ChatbotMessage {
            id: ChatbotMessageId::generate(),
            thread_id: thread.clone(),
            user_id: user.clone(),
            role: crate::domain::chatbot_message::MessageRole::User,
            content,
            status: MessageStatus::Complete,
            truncated: false,
            error_code: None,
            created_at: now,
            completed_at: Some(now),
        },
    )
    .await
}

/// Reserve the assistant's answer *before* the AI call: the row exists,
/// empty and `pending`, so a reload finds the turn and can wait on it.
pub async fn append_pending_assistant(
    db: &Database,
    thread: &ChatbotThreadId,
    user: &UserId,
) -> Result<ChatbotMessage, AppError> {
    insert(
        db,
        ChatbotMessage {
            id: ChatbotMessageId::generate(),
            thread_id: thread.clone(),
            user_id: user.clone(),
            role: crate::domain::chatbot_message::MessageRole::Assistant,
            content: ChatContent::empty(),
            status: MessageStatus::Pending,
            truncated: false,
            error_code: None,
            created_at: Timestamp::now(),
            completed_at: None,
        },
    )
    .await
}

/// The whole thread, oldest first — the order both the UI and the AI
/// history payload read in.
pub async fn list_for_thread(
    db: &Database,
    thread: &ChatbotThreadId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ChatbotMessage>, i64), AppError> {
    let (rows, total) = PagedList::new(
        "chatbot_message WHERE thread_id = $1",
        "ORDER BY created_at ASC, id ASC",
    )
    .bind(thread.uuid())
    .run::<ChatbotMessage>(limit, offset, db)
    .await?;
    Ok((
        rows.into_iter().map(ChatbotMessage::projected).collect(),
        total,
    ))
}

/// The last `limit` turns, still oldest-first — the tail replayed to the
/// AI service as context. Taken newest-first in the database (so the
/// `LIMIT` keeps the *recent* end) and flipped back here.
pub async fn list_tail(
    db: &Database,
    thread: &ChatbotThreadId,
    limit: usize,
) -> Result<Vec<ChatbotMessage>, AppError> {
    let mut messages: Vec<ChatbotMessage> = query_as!(
        ChatbotMessage,
        "SELECT id AS \"id: ChatbotMessageId\", thread_id AS \"thread_id: ChatbotThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: MessageRole\", content AS \"content: ChatContent\", \
                status AS \"status: MessageStatus\", truncated, \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM chatbot_message WHERE thread_id = $1 \
         ORDER BY created_at DESC, id DESC LIMIT $2",
        thread.uuid(),
        limit as i64
    )
    .fetch_all(db)
    .await?;
    messages.reverse();
    Ok(messages)
}

/// The last `limit` *settled* turns that carry text, still oldest-first —
/// the tail replayed to the AI service as context. The filter runs in the
/// query, not after it: a `LIMIT` over the raw tail hands back fewer usable
/// rows the moment a run of answers fails, silently shrinking the context
/// instead of reaching further back. No projection is applied — it only
/// ever rewrites a `pending` row, and none is selected here.
pub async fn list_settled_tail(
    db: &Database,
    thread: &ChatbotThreadId,
    limit: usize,
) -> Result<Vec<ChatbotMessage>, AppError> {
    let mut messages: Vec<ChatbotMessage> = query_as!(
        ChatbotMessage,
        "SELECT id AS \"id: ChatbotMessageId\", thread_id AS \"thread_id: ChatbotThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: MessageRole\", content AS \"content: ChatContent\", \
                status AS \"status: MessageStatus\", truncated, \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM chatbot_message WHERE thread_id = $1 \
           AND status = 'complete' AND content <> '' \
         ORDER BY created_at DESC, id DESC LIMIT $2",
        thread.uuid(),
        limit as i64
    )
    .fetch_all(db)
    .await?;
    messages.reverse();
    Ok(messages)
}

/// The user turn a reserved answer belongs to, by *identity*: the prompt
/// row of the very POST that reserved it, whose id the answering task
/// carries.
///
/// Never derived from write order. Two POSTs on one thread interleave
/// across the two creates — the rows land `userA, userB, asstA, asstB` —
/// so "the newest user row written before this answer" resolves *both*
/// answers to prompt B, and prompt A is never answered.
pub async fn prompt_of(
    db: &Database,
    id: &ChatbotMessageId,
) -> Result<Option<ChatbotMessage>, AppError> {
    let message = query_as!(
        ChatbotMessage,
        "SELECT id AS \"id: ChatbotMessageId\", thread_id AS \"thread_id: ChatbotThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: MessageRole\", content AS \"content: ChatContent\", \
                status AS \"status: MessageStatus\", truncated, \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM chatbot_message WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(message)
}

/// Read one turn only if `user` owns it — the poll loop's read.
pub async fn read_for(
    db: &Database,
    id: &ChatbotMessageId,
    user: &UserId,
) -> Result<Option<ChatbotMessage>, AppError> {
    let message = query_as!(
        ChatbotMessage,
        "SELECT id AS \"id: ChatbotMessageId\", thread_id AS \"thread_id: ChatbotThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: MessageRole\", content AS \"content: ChatContent\", \
                status AS \"status: MessageStatus\", truncated, \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM chatbot_message WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(message
        .filter(|message| &message.user_id == user)
        .map(ChatbotMessage::projected))
}

/// Land the answer, recording whether it had to be clipped to fit the
/// school's cap. Field-scoped and gated on `status = 'pending'` in the
/// `WHERE`, so a late reply can't overwrite a row the boot sweep (or a
/// timeout) already failed, and two answers can't both apply.
pub async fn complete(
    db: &Database,
    id: &ChatbotMessageId,
    text: ChatContent,
    truncated: bool,
) -> Result<ChatbotMessage, AppError> {
    settle(db, id, Some(text), truncated, None).await
}

/// Mark the answer failed with a short code (`unavailable`, the service's
/// own error code, …). Same pending gate as [`complete`].
pub async fn fail(
    db: &Database,
    id: &ChatbotMessageId,
    error_code: &str,
) -> Result<ChatbotMessage, AppError> {
    // A failed turn has no text, so there is nothing that could be clipped.
    settle(db, id, None, false, Some(error_code)).await
}

async fn settle(
    db: &Database,
    id: &ChatbotMessageId,
    text: Option<ChatContent>,
    truncated: bool,
    error_code: Option<&str>,
) -> Result<ChatbotMessage, AppError> {
    let status = if text.is_some() {
        MessageStatus::Complete
    } else {
        MessageStatus::Failed
    };
    let settled = query_as!(
        ChatbotMessage,
        "UPDATE chatbot_message SET content = $2, status = $3, truncated = $4, \
             error_code = $5, completed_at = $6 \
         WHERE id = $1 AND status = 'pending' \
         RETURNING id AS \"id: ChatbotMessageId\", thread_id AS \"thread_id: ChatbotThreadId\", \
                   user_id AS \"user_id: UserId\", role AS \"role: MessageRole\", content AS \"content: ChatContent\", \
                   status AS \"status: MessageStatus\", truncated, \
                   error_code AS \"error_code: String\", \
                   created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\"",
        id.uuid(),
        text.map(|t| t.as_str().to_string()).unwrap_or_default(),
        status.as_str(),
        truncated,
        error_code.map(|code| code.chars().take(MAX_ERROR_CODE_LEN).collect::<String>()),
        Timestamp::now().as_millis(),
    )
    .fetch_optional(db)
    .await?;
    settled.ok_or(AppError::NotFound)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::chatbot_message::MessageRole;

    #[tokio::test]
    async fn a_turns_two_rows_never_sort_inverted() {
        // Both rows of a turn land in the same millisecond routinely, so the
        // `id` tie-break decides the thread's order. With a random ULID this
        // inverted a fifth of the pairs; here every pair must read back
        // question-then-answer, from both read paths.
        let db = crate::database::init_mem().await.unwrap();
        // A real thread row: every turn is written through it, so a turn with
        // no thread is refused (see [`insert`]).
        db.query("CREATE chatbot_thread:c SET user_id = user:u, created_at = 0, updated_at = 0")
            .await
            .unwrap()
            .check()
            .unwrap();
        let thread = ChatbotThreadId::from_key("c");
        let user = UserId::from_key("u");
        const TURNS: usize = 200;

        for turn in 0..TURNS {
            let content = ChatContent::try_new(&format!("soru {turn}")).unwrap();
            append_user(&db, &thread, &user, content).await.unwrap();
            append_pending_assistant(&db, &thread, &user).await.unwrap();
        }

        let (whole, _) = list_for_thread(&db, &thread, None, 0).await.unwrap();
        let tail = list_tail(&db, &thread, TURNS * 2).await.unwrap();
        for messages in [&whole, &tail] {
            assert_eq!(messages.len(), TURNS * 2);
            for (turn, pair) in messages.chunks(2).enumerate() {
                assert_eq!(pair[0].get_role(), MessageRole::User, "turn {turn}");
                assert_eq!(pair[0].get_content().as_str(), format!("soru {turn}"));
                assert_eq!(pair[1].get_role(), MessageRole::Assistant, "turn {turn}");
            }
        }
    }
}
