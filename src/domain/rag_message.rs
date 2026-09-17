//! One turn of a RAG thread — the row shape and its read-time presentation.
//! `user_id` is duplicated from the thread onto every message so an ownership
//! check is one read, with no join.
//!
//! Persistence lives in [`crate::db::rag_message`]: the assistant's row is
//! written `pending` *before* the AI call and flipped to `complete` or
//! `failed` by the task that owns the bridge round trip, and a read projects a
//! long-stale `pending` as failed without writing
//! ([`RagMessage::projected`]) — the durable repair for a dead process happens
//! once, at mint, in `database.rs`'s school migration sweep.
//!
//! What separates this row from the chatbot's is `reply`: a RAG answer is
//! stored with the service's own JSON — whether it abstained, why, and which
//! passages it cited — so a reader can open the evidence behind the text. A
//! failed or pending turn stores none.

use crate::constant::{MAX_CHATBOT_MESSAGE_LEN, RAG_PENDING_STALE_SECS, STALE_ERROR_CODE};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::rag_thread::RagThreadId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct RagMessageId(uuid::Uuid);

impl RagMessageId {
    /// Mints in write order. Unlike a plain random UUID, whose random low bits
    /// sort arbitrarily among ids minted in the same millisecond, the
    /// process-wide context increments — so the `id` tie-break in the
    /// `ORDER BY` of
    /// [`list_for_thread`](crate::db::rag_message::list_for_thread) /
    /// [`list_tail`](crate::db::rag_message::list_tail) is the
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
    pub fn uuid(&self) -> uuid::Uuid {
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
pub enum RagMessageRole {
    User,
    Assistant,
}

impl RagMessageRole {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            RagMessageRole::User => "user",
            RagMessageRole::Assistant => "assistant",
        }
    }
}

/// Where an assistant turn is in its lifecycle. A user turn is born
/// `Complete` — nothing is ever awaited for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum RagMessageStatus {
    Pending,
    Complete,
    Failed,
}

impl RagMessageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RagMessageStatus::Pending => "pending",
            RagMessageStatus::Complete => "complete",
            RagMessageStatus::Failed => "failed",
        }
    }
}

/// One turn's text. The hard ceiling only, and the chatbot's
/// ([`MAX_CHATBOT_MESSAGE_LEN`]): a question is a pasted prompt in either
/// nest, and the school-adjustable `max_chatbot_message_len` is the web
/// layer's to enforce, below this.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct RagContent(String);

impl RagContent {
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

/// One passage an answer drew on, as stored on the row. `n` is the marker the
/// answer text points at (`[N]` resolves to the citation whose `n` is `N`);
/// `file` is the `course_note_file` record key the service's `doc_id` was
/// resolved through, absent while no file claims that document. Stored as
/// JSON — this is the service's shape, kept, not re-derived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagCitedFile {
    pub n: u32,
    pub file: Option<String>,
    pub pages: Vec<i64>,
    pub span_ids: Vec<String>,
    pub ders: Option<String>,
}

/// A RAG answer besides its text: whether the service declined to answer
/// rather than answering, why, and the passages it drew on. Stored in the
/// message's `reply` column, so a settled turn's evidence survives the
/// request that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagReply {
    /// A true value is a complete, successful turn — the service chose not to
    /// answer — not a failure, and `reason` says why.
    pub abstained: bool,
    /// The abstention's short machine code (`""` on an ordinary answer).
    pub reason: String,
    pub citations: Vec<RagCitedFile>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RagMessage {
    pub(crate) id: RagMessageId,
    pub(crate) thread: RagThreadId,
    pub(crate) user_id: UserId,
    pub(crate) role: RagMessageRole,
    pub(crate) status: RagMessageStatus,
    pub(crate) content: RagContent,
    /// The service's answer beside the text; `None` on a pending row, on a
    /// failed one, and on every user turn.
    pub(crate) reply: Option<Json<RagReply>>,
    pub(crate) error_code: Option<String>,
    pub(crate) created_at: Timestamp,
    pub(crate) completed_at: Option<Timestamp>,
}

impl RagMessage {
    pub fn get_id(&self) -> &RagMessageId {
        &self.id
    }

    pub fn get_thread_id(&self) -> &RagThreadId {
        &self.thread
    }

    pub fn get_user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn get_role(&self) -> RagMessageRole {
        self.role
    }

    pub fn get_content(&self) -> &RagContent {
        &self.content
    }

    pub fn get_status(&self) -> RagMessageStatus {
        self.status
    }

    pub fn get_reply(&self) -> Option<&RagReply> {
        self.reply.as_ref().map(|reply| &reply.0)
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

    /// A row still `pending` past [`RAG_PENDING_STALE_SECS`] has lost the task
    /// that owed it an answer; present it as failed.
    ///
    /// Read-time projection, not a lazy write-back: the read paths (poll loop,
    /// thread load) run on every page and must not write, a write-back would
    /// race the answering task that is merely slow — closing the door on a
    /// reply that is still coming — and the durable repair for the real cause
    /// (a dead process) already happens once, at mint. The stored row stays
    /// truthful; only the answer handed to the caller is projected.
    pub(crate) fn projected(mut self) -> Self {
        let stale_at = self.created_at.as_millis() + RAG_PENDING_STALE_SECS * 1_000;
        if self.status == RagMessageStatus::Pending && Timestamp::now().as_millis() > stale_at {
            self.status = RagMessageStatus::Failed;
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
        assert!(RagContent::try_new("").is_err());
        assert!(RagContent::try_new(&"x".repeat(MAX_CHATBOT_MESSAGE_LEN)).is_ok());
        assert!(RagContent::try_new(&"x".repeat(MAX_CHATBOT_MESSAGE_LEN + 1)).is_err());
    }

    /// The `role`/`status` columns are `TEXT` with CHECKs listing exactly
    /// these spellings — queries match on them, so the storage form may not
    /// drift from `as_str` (which `rename_all` mirrors).
    #[test]
    fn role_and_status_spellings_are_frozen() {
        assert_eq!(RagMessageRole::User.as_str(), "user");
        assert_eq!(RagMessageRole::Assistant.as_str(), "assistant");
        assert_eq!(RagMessageStatus::Pending.as_str(), "pending");
        assert_eq!(RagMessageStatus::Complete.as_str(), "complete");
        assert_eq!(RagMessageStatus::Failed.as_str(), "failed");
    }

    /// The reply is stored as JSONB and handed to readers verbatim, so its key
    /// names are a wire contract: a rename here would silently change what a
    /// stored answer still says to every client (and the frontend is not the
    /// thing that moves).
    #[test]
    fn a_reply_serializes_under_the_keys_a_reader_reads() {
        let reply = RagReply {
            abstained: false,
            reason: String::new(),
            citations: vec![RagCitedFile {
                n: 1,
                file: Some("019732e3-7b00-7000-8000-00000000f11e".to_string()),
                pages: vec![3],
                span_ids: vec!["s1".to_string()],
                ders: Some("Fizik".to_string()),
            }],
        };
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            serde_json::json!({
                "abstained": false,
                "reason": "",
                "citations": [{
                    "n": 1,
                    "file": "019732e3-7b00-7000-8000-00000000f11e",
                    "pages": [3],
                    "span_ids": ["s1"],
                    "ders": "Fizik",
                }],
            })
        );
    }

    #[tokio::test]
    async fn stale_pending_projects_as_failed() {
        let aged = |secs: i64| {
            RagMessage {
                id: RagMessageId::generate(),
                thread: RagThreadId::from_key("0198f1a2-3b4c-7d5e-8f90-1a2b3c4d5e6f"),
                user_id: UserId::from_key("0198f1a2-3b4c-7d5e-8f90-aa2b3c4d5e6f"),
                role: RagMessageRole::Assistant,
                content: RagContent::empty(),
                status: RagMessageStatus::Pending,
                reply: None,
                error_code: None,
                created_at: Timestamp::from_millis(Timestamp::now().as_millis() - secs * 1_000),
                completed_at: None,
            }
            .projected()
        };

        let fresh = aged(1);
        assert_eq!(fresh.get_status(), RagMessageStatus::Pending);
        assert_eq!(fresh.get_error_code(), None);

        let stale = aged(RAG_PENDING_STALE_SECS + 1);
        assert_eq!(stale.get_status(), RagMessageStatus::Failed);
        assert_eq!(stale.get_error_code(), Some(STALE_ERROR_CODE));
    }
}
