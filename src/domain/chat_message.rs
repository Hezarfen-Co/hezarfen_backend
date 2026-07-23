//! One turn of a chatbot thread. Both sides are persisted: the user's prompt
//! lands `complete`, and the assistant's row is written `pending` *before* the
//! AI call so a reload never loses an answer in flight. The task that owns the
//! bridge stream then flips it to `complete` (with the text) or `failed` (with
//! a code) — and if that task dies with the process, the boot sweep in
//! `database.rs` fails the row instead.
//!
//! `user_id` is duplicated from the conversation onto every message so an
//! ownership check is one read, with no join.

use std::sync::{LazyLock, Mutex};

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::{Generator, Ulid};

use crate::constant::{CHAT_PENDING_STALE_SECS, MAX_CHAT_MESSAGE_LEN};
use crate::database::{CHAT_MESSAGE_TABLE, Database};
use crate::domain::conversation::ConversationId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// The `error_code` a stale `pending` row presents as. Distinct from the boot
/// sweep's `interrupted`: this one was never repaired, only projected.
pub const STALE_ERROR_CODE: &str = "timed_out";

/// Ceiling on a stored `error_code`. Codes are short slugs, but one arrives
/// from an out-of-process AI service — a trust boundary — so it is trimmed
/// rather than trusted.
const MAX_ERROR_CODE_LEN: usize = 64;

/// Mints message ids in write order. Unlike `Ulid::new()`, whose 80 random
/// low bits sort arbitrarily among ids minted in the same millisecond, this
/// increments the previous id — so the `id` tie-break in the `ORDER BY` of
/// [`ChatMessage::list_for_conversation`] / [`ChatMessage::list_tail`] is the
/// order the rows were written. The user prompt and the assistant row one POST
/// writes back-to-back routinely share a millisecond, and a random tie-break
/// there renders the answer *above* its own question — and hands the AI
/// service a `[assistant, user]` history, breaking the oldest-first contract.
static IDS: LazyLock<Mutex<Generator>> = LazyLock::new(|| Mutex::new(Generator::new()));

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatMessageId(RecordId);

impl ChatMessageId {
    pub fn generate() -> Self {
        let mut ids = IDS.lock().expect("chat id generator poisoned");
        // The only error is overflow of the random bits *within* one
        // millisecond — 2^80 ids deep, and it clears itself as soon as the
        // clock ticks over, so retry rather than fall back to a random id
        // (which would silently reintroduce the defect).
        let ulid: Ulid = loop {
            if let Ok(ulid) = ids.generate() {
                break ulid;
            }
        };
        Self(RecordId::new(CHAT_MESSAGE_TABLE, ulid.to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(CHAT_MESSAGE_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// Who said a turn. `untagged` + `rename_all` store it as the bare lowercase
/// string the `role` column types as, and a value the enum doesn't know comes
/// back as a deserialization error, never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }
}

/// Where an assistant turn is in its lifecycle. A user turn is born
/// `Complete` — nothing is ever awaited for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum MessageStatus {
    Pending,
    Complete,
    Failed,
}

impl MessageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageStatus::Pending => "pending",
            MessageStatus::Complete => "complete",
            MessageStatus::Failed => "failed",
        }
    }
}

/// One turn's text. The hard ceiling only — the school-adjustable
/// `max_chat_message_len` is the web layer's to enforce, below this.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatContent(String);

impl ChatContent {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("content", value, MAX_CHAT_MESSAGE_LEN)?;
        Ok(Self(value.to_string()))
    }

    /// The placeholder a `pending` assistant row carries until its answer
    /// lands. Private: every content that comes from outside is non-empty.
    fn empty() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct ChatMessage {
    id: ChatMessageId,
    conversation_id: ConversationId,
    user_id: UserId,
    role: MessageRole,
    content: ChatContent,
    status: MessageStatus,
    /// Whether the stored `content` is a clipped version of what the AI
    /// service actually answered. Only an assistant turn can ever set it: a
    /// user prompt over the cap is refused (400), never trimmed.
    truncated: bool,
    error_code: Option<String>,
    created_at: Timestamp,
    completed_at: Option<Timestamp>,
}

impl ChatMessage {
    pub fn get_id(&self) -> &ChatMessageId {
        &self.id
    }

    pub fn get_conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }

    pub fn get_user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn get_role(&self) -> MessageRole {
        self.role
    }

    pub fn get_content(&self) -> &ChatContent {
        &self.content
    }

    pub fn get_status(&self) -> MessageStatus {
        self.status
    }

    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn get_error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn get_completed_at(&self) -> Option<Timestamp> {
        self.completed_at
    }

    /// A row still `pending` past [`CHAT_PENDING_STALE_SECS`] has lost the task
    /// that owed it an answer; present it as failed.
    ///
    /// Read-time projection, not a lazy write-back: the read paths (poll loop,
    /// thread load) run on every page and must not write, a write-back would
    /// race the answering task that is merely slow — closing the door on a
    /// reply that is still coming — and the durable repair for the real cause
    /// (a dead process) already happens once, at boot. The stored row stays
    /// truthful; only the answer handed to the caller is projected.
    fn projected(mut self) -> Self {
        let stale_at = self.created_at.as_millis() + CHAT_PENDING_STALE_SECS * 1_000;
        if self.status == MessageStatus::Pending && Timestamp::now().as_millis() > stale_at {
            self.status = MessageStatus::Failed;
            self.error_code = Some(STALE_ERROR_CODE.to_string());
        }
        self
    }

    async fn insert(message: ChatMessage, db: &Database) -> Result<ChatMessage, AppError> {
        let created: Option<ChatMessage> = db.create(message.id.record()).content(message).await?;
        created.ok_or_else(|| AppError::Internal("failed to create chat message".into()))
    }

    /// Append the user's prompt. Nothing is awaited for it, so it is born
    /// complete.
    pub async fn append_user(
        conversation: &ConversationId,
        user: &UserId,
        content: ChatContent,
        db: &Database,
    ) -> Result<ChatMessage, AppError> {
        let now = Timestamp::now();
        Self::insert(
            ChatMessage {
                id: ChatMessageId::generate(),
                conversation_id: conversation.clone(),
                user_id: user.clone(),
                role: MessageRole::User,
                content,
                status: MessageStatus::Complete,
                truncated: false,
                error_code: None,
                created_at: now,
                completed_at: Some(now),
            },
            db,
        )
        .await
    }

    /// Reserve the assistant's answer *before* the AI call: the row exists,
    /// empty and `pending`, so a reload finds the turn and can wait on it.
    pub async fn append_pending_assistant(
        conversation: &ConversationId,
        user: &UserId,
        db: &Database,
    ) -> Result<ChatMessage, AppError> {
        Self::insert(
            ChatMessage {
                id: ChatMessageId::generate(),
                conversation_id: conversation.clone(),
                user_id: user.clone(),
                role: MessageRole::Assistant,
                content: ChatContent::empty(),
                status: MessageStatus::Pending,
                truncated: false,
                error_code: None,
                created_at: Timestamp::now(),
                completed_at: None,
            },
            db,
        )
        .await
    }

    /// The whole thread, oldest first — the order both the UI and the AI
    /// history payload read in.
    pub async fn list_for_conversation(
        conversation: &ConversationId,
        db: &Database,
    ) -> Result<Vec<ChatMessage>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM chat_message WHERE conversation_id = $conv \
                 ORDER BY created_at ASC, id ASC",
            )
            .bind(("conv", conversation.record()))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<ChatMessage>>(0)?
            .into_iter()
            .map(ChatMessage::projected)
            .collect())
    }

    /// The last `limit` turns, still oldest-first — the tail replayed to the
    /// AI service as context. Taken newest-first in the database (so the
    /// `LIMIT` keeps the *recent* end) and flipped back here.
    pub async fn list_tail(
        conversation: &ConversationId,
        limit: usize,
        db: &Database,
    ) -> Result<Vec<ChatMessage>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM chat_message WHERE conversation_id = $conv \
                 ORDER BY created_at DESC, id DESC LIMIT $limit",
            )
            .bind(("conv", conversation.record()))
            .bind(("limit", limit as i64))
            .await?
            .check()?;
        let mut messages: Vec<ChatMessage> = result
            .take::<Vec<ChatMessage>>(0)?
            .into_iter()
            .map(ChatMessage::projected)
            .collect();
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
        conversation: &ConversationId,
        limit: usize,
        db: &Database,
    ) -> Result<Vec<ChatMessage>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM chat_message WHERE conversation_id = $conv \
                 AND status = 'complete' AND content != '' \
                 ORDER BY created_at DESC, id DESC LIMIT $limit",
            )
            .bind(("conv", conversation.record()))
            .bind(("limit", limit as i64))
            .await?
            .check()?;
        let mut messages: Vec<ChatMessage> = result.take(0)?;
        messages.reverse();
        Ok(messages)
    }

    /// Read one turn only if `user` owns it — the poll loop's read.
    pub async fn read_for(
        id: &ChatMessageId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ChatMessage>, AppError> {
        let message: Option<ChatMessage> = db.select(id.record()).await?;
        Ok(message
            .filter(|message| &message.user_id == user)
            .map(ChatMessage::projected))
    }

    /// Land the answer, recording whether it had to be clipped to fit the
    /// school's cap. Field-scoped and gated on `status = 'pending'` in the
    /// `WHERE`, so a late reply can't overwrite a row the boot sweep (or a
    /// timeout) already failed, and two answers can't both apply.
    pub async fn complete(
        id: &ChatMessageId,
        text: ChatContent,
        truncated: bool,
        db: &Database,
    ) -> Result<ChatMessage, AppError> {
        Self::settle(id, Some(text), truncated, None, db).await
    }

    /// Mark the answer failed with a short code (`unavailable`, the service's
    /// own error code, …). Same pending gate as [`ChatMessage::complete`].
    pub async fn fail(
        id: &ChatMessageId,
        error_code: &str,
        db: &Database,
    ) -> Result<ChatMessage, AppError> {
        // A failed turn has no text, so there is nothing that could be clipped.
        Self::settle(id, None, false, Some(error_code), db).await
    }

    async fn settle(
        id: &ChatMessageId,
        text: Option<ChatContent>,
        truncated: bool,
        error_code: Option<&str>,
        db: &Database,
    ) -> Result<ChatMessage, AppError> {
        let status = if text.is_some() {
            MessageStatus::Complete
        } else {
            MessageStatus::Failed
        };
        let mut result = db
            .query(
                "UPDATE $id SET content = $content, status = $status, truncated = $truncated, \
                 error_code = $code, completed_at = $now WHERE status = 'pending' RETURN AFTER",
            )
            .bind(("id", id.record()))
            .bind(("content", text.map(|t| t.0).unwrap_or_default()))
            .bind(("status", status.as_str().to_string()))
            .bind(("truncated", truncated))
            .bind((
                "code",
                error_code.map(|code| code.chars().take(MAX_ERROR_CODE_LEN).collect::<String>()),
            ))
            .bind(("now", Timestamp::now().as_millis()))
            .await?
            .check()?;
        result
            .take::<Vec<ChatMessage>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Value;

    #[tokio::test]
    async fn content_is_required_and_capped() {
        assert!(ChatContent::try_new("").is_err());
        assert!(ChatContent::try_new(&"x".repeat(MAX_CHAT_MESSAGE_LEN)).is_ok());
        assert!(ChatContent::try_new(&"x".repeat(MAX_CHAT_MESSAGE_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn role_and_status_store_as_bare_strings() {
        // `TYPE string` columns: an object-wrapped enum would be rejected on
        // write, and a junk string must read back as an error, not a panic.
        for role in [MessageRole::User, MessageRole::Assistant] {
            let value = role.into_value();
            assert_eq!(value, Value::String(role.as_str().to_string()));
            assert_eq!(MessageRole::from_value(value).unwrap(), role);
        }
        for status in [
            MessageStatus::Pending,
            MessageStatus::Complete,
            MessageStatus::Failed,
        ] {
            let value = status.into_value();
            assert_eq!(value, Value::String(status.as_str().to_string()));
            assert_eq!(MessageStatus::from_value(value).unwrap(), status);
        }
        assert!(MessageRole::from_value(Value::String("system".into())).is_err());
        assert!(MessageStatus::from_value(Value::String("queued".into())).is_err());
    }

    #[tokio::test]
    async fn a_turns_two_rows_never_sort_inverted() {
        // Both rows of a turn land in the same millisecond routinely, so the
        // `id` tie-break decides the thread's order. With a random ULID this
        // inverted a fifth of the pairs; here every pair must read back
        // question-then-answer, from both read paths.
        let db = crate::database::init_mem().await.unwrap();
        let conversation = ConversationId::from_key("c");
        let user = UserId::from_key("u");
        const TURNS: usize = 200;

        for turn in 0..TURNS {
            let content = ChatContent::try_new(&format!("soru {turn}")).unwrap();
            ChatMessage::append_user(&conversation, &user, content, &db)
                .await
                .unwrap();
            ChatMessage::append_pending_assistant(&conversation, &user, &db)
                .await
                .unwrap();
        }

        let whole = ChatMessage::list_for_conversation(&conversation, &db)
            .await
            .unwrap();
        let tail = ChatMessage::list_tail(&conversation, TURNS * 2, &db)
            .await
            .unwrap();
        for messages in [&whole, &tail] {
            assert_eq!(messages.len(), TURNS * 2);
            for (turn, pair) in messages.chunks(2).enumerate() {
                assert_eq!(pair[0].get_role(), MessageRole::User, "turn {turn}");
                assert_eq!(pair[0].get_content().as_str(), format!("soru {turn}"));
                assert_eq!(pair[1].get_role(), MessageRole::Assistant, "turn {turn}");
            }
        }
    }

    #[tokio::test]
    async fn stale_pending_projects_as_failed() {
        let aged = |secs: i64| {
            ChatMessage {
                id: ChatMessageId::generate(),
                conversation_id: ConversationId::from_key("c"),
                user_id: UserId::from_key("u"),
                role: MessageRole::Assistant,
                content: ChatContent::empty(),
                status: MessageStatus::Pending,
                truncated: false,
                error_code: None,
                created_at: Timestamp::from_millis(Timestamp::now().as_millis() - secs * 1_000),
                completed_at: None,
            }
            .projected()
        };

        let fresh = aged(1);
        assert_eq!(fresh.get_status(), MessageStatus::Pending);
        assert_eq!(fresh.get_error_code(), None);

        let stale = aged(CHAT_PENDING_STALE_SECS + 1);
        assert_eq!(stale.get_status(), MessageStatus::Failed);
        assert_eq!(stale.get_error_code(), Some(STALE_ERROR_CODE));
    }
}
