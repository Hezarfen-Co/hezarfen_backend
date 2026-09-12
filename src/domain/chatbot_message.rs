//! One turn of a chatbot thread — the row shape and its read-time
//! presentation. `user_id` is duplicated from the thread onto every message
//! so an ownership check is one read, with no join.
//!
//! Persistence lives in [`crate::db::chatbot_message`]: the assistant's row
//! is written `pending` *before* the AI call and flipped to `complete` or
//! `failed` by the task that owns the bridge round trip, and a read
//! projects a long-stale `pending` as failed without writing
//! ([`ChatbotMessage::projected`]) — the durable repair for a dead process
//! happens once, at mint, in `database.rs`'s school migration sweep.

use uuid::Uuid;
use crate::constant::{
    CHATBOT_PENDING_STALE_SECS, MAX_CHATBOT_MESSAGE_LEN, STALE_ERROR_CODE,
};
use crate::domain::chatbot_thread::ChatbotThreadId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ChatbotMessageId(uuid::Uuid);

impl ChatbotMessageId {
    /// Mints in write order. Unlike a plain random UUID, whose random low bits
    /// sort arbitrarily among ids minted in the same millisecond, the
    /// process-wide context increments — so the `id` tie-break in the
    /// `ORDER BY` of
    /// [`list_for_thread`](crate::db::chatbot_message::list_for_thread) /
    /// [`list_tail`](crate::db::chatbot_message::list_tail) is the
    /// order the rows were written. The user prompt and the assistant row one
    /// POST writes back-to-back routinely share a millisecond, and a random
    /// tie-break there renders the answer *above* its own question — and
    /// hands the AI service a `[assistant, user]` history, breaking the
    /// oldest-first contract.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// Who said a turn. Stored as the bare lowercase string the `role` column's
/// CHECK allows; a value the enum doesn't know comes back as a decode error,
/// never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
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
/// `max_chatbot_message_len` is the web layer's to enforce, below this.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ChatContent(String);

impl ChatContent {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("content", value, MAX_CHATBOT_MESSAGE_LEN)?;
        Ok(Self(value.to_string()))
    }

    /// The placeholder a `pending` assistant row carries until its answer
    /// lands. Crate-visible: only the persistence layer writes it, for the
    /// reserved row before the answer exists.
    pub(crate) fn empty() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatbotMessage {
    pub(crate) id: ChatbotMessageId,
    pub(crate) thread_id: ChatbotThreadId,
    pub(crate) user_id: UserId,
    pub(crate) role: MessageRole,
    pub(crate) content: ChatContent,
    pub(crate) status: MessageStatus,
    /// Whether the stored `content` is a clipped version of what the AI
    /// service actually answered. Only an assistant turn can ever set it: a
    /// user prompt over the cap is refused (400), never trimmed.
    pub(crate) truncated: bool,
    pub(crate) error_code: Option<String>,
    pub(crate) created_at: Timestamp,
    pub(crate) completed_at: Option<Timestamp>,
}

impl ChatbotMessage {
    pub fn get_id(&self) -> &ChatbotMessageId {
        &self.id
    }

    pub fn get_thread_id(&self) -> &ChatbotThreadId {
        &self.thread_id
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

    /// A row still `pending` past [`CHATBOT_PENDING_STALE_SECS`] has lost the task
    /// that owed it an answer; present it as failed.
    ///
    /// Read-time projection, not a lazy write-back: the read paths (poll loop,
    /// thread load) run on every page and must not write, a write-back would
    /// race the answering task that is merely slow — closing the door on a
    /// reply that is still coming — and the durable repair for the real cause
    /// (a dead process) already happens once, at mint. The stored row stays
    /// truthful; only the answer handed to the caller is projected.
    pub(crate) fn projected(mut self) -> Self {
        let stale_at = self.created_at.as_millis() + CHATBOT_PENDING_STALE_SECS * 1_000;
        if self.status == MessageStatus::Pending && Timestamp::now().as_millis() > stale_at {
            self.status = MessageStatus::Failed;
            self.error_code = Some(STALE_ERROR_CODE.to_string());
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn content_is_required_and_capped() {
        assert!(ChatContent::try_new("").is_err());
        assert!(ChatContent::try_new(&"x".repeat(MAX_CHATBOT_MESSAGE_LEN)).is_ok());
        assert!(ChatContent::try_new(&"x".repeat(MAX_CHATBOT_MESSAGE_LEN + 1)).is_err());
    }

    /// The `role`/`status` columns are `TEXT` with CHECKs listing exactly
    /// these spellings — queries match on them, so the storage form may not
    /// drift from `as_str` (which `rename_all` mirrors).
    #[test]
    fn role_and_status_spellings_are_frozen() {
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
        assert_eq!(MessageStatus::Pending.as_str(), "pending");
        assert_eq!(MessageStatus::Complete.as_str(), "complete");
        assert_eq!(MessageStatus::Failed.as_str(), "failed");
    }

    #[tokio::test]
    async fn stale_pending_projects_as_failed() {
        let aged = |secs: i64| {
            ChatbotMessage {
                id: ChatbotMessageId::generate(),
                thread_id: ChatbotThreadId::from_key("0198f1a2-3b4c-7d5e-8f90-1a2b3c4d5e6f"),
                user_id: UserId::from_key("0198f1a2-3b4c-7d5e-8f90-aa2b3c4d5e6f"),
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

        let stale = aged(CHATBOT_PENDING_STALE_SECS + 1);
        assert_eq!(stale.get_status(), MessageStatus::Failed);
        assert_eq!(stale.get_error_code(), Some(STALE_ERROR_CODE));
    }
}
