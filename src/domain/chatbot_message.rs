//! One turn of a chatbot thread. Both sides are persisted: the user's prompt
//! lands `complete`, and the assistant's row is written `pending` *before* the
//! AI call so a reload never loses an answer in flight. A pending row is then
//! a durable *job*: whichever replica holds a `chat.reply` worker claims it
//! (`claim_pending`), dispatches it, and flips it to `complete` (with the text)
//! or `failed` (with a code). A claimer that dies loses its claim to the next
//! sweep, and the boot sweep in `database.rs` fails whatever nobody could
//! answer inside the staleness window.
//!
//! `user_id` is duplicated from the thread onto every message so an
//! ownership check is one read, with no join.

use std::sync::{LazyLock, Mutex};

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::{Generator, Ulid};

use crate::constant::{
    CHAT_MESSAGE_TABLE, CHATBOT_CLAIM, CHATBOT_CLAIM_BATCH, CHATBOT_CLAIM_RECLAIM_SECS,
    CHATBOT_PENDING_STALE_SECS, MAX_CHATBOT_MESSAGE_LEN, MAX_ERROR_CODE_LEN, STALE_ERROR_CODE,
};
use crate::database::Database;
use crate::domain::chatbot_thread::ChatbotThreadId;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Mints message ids in write order. Unlike `Ulid::new()`, whose 80 random
/// low bits sort arbitrarily among ids minted in the same millisecond, this
/// increments the previous id — so the `id` tie-break in the `ORDER BY` of
/// [`ChatbotMessage::list_for_thread`] / [`ChatbotMessage::list_tail`] is the
/// order the rows were written. The user prompt and the assistant row one POST
/// writes back-to-back routinely share a millisecond, and a random tie-break
/// there renders the answer *above* its own question — and hands the AI
/// service a `[assistant, user]` history, breaking the oldest-first contract.
static IDS: LazyLock<Mutex<Generator>> = LazyLock::new(|| Mutex::new(Generator::new()));

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatbotMessageId(RecordId);

impl ChatbotMessageId {
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
/// `max_chatbot_message_len` is the web layer's to enforce, below this.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatContent(String);

impl ChatContent {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("content", value, MAX_CHATBOT_MESSAGE_LEN)?;
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
pub struct ChatbotMessage {
    id: ChatbotMessageId,
    thread_id: ChatbotThreadId,
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
    /// Which replica's claim loop owes this turn an answer, and when it said
    /// so. `None` means nobody has taken it; a claim older than
    /// [`CHATBOT_CLAIM_RECLAIM_SECS`] is a claimer that died, and the turn goes
    /// back on the queue (see [`ChatbotMessage::claim_pending`]).
    ///
    /// Never read by an HTTP handler and never in the DTO: whose process is
    /// answering is deployment plumbing, not something a client can act on.
    claimed_by: Option<String>,
    claimed_at: Option<Timestamp>,
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
    /// (a dead process) already happens once, at boot. The stored row stays
    /// truthful; only the answer handed to the caller is projected.
    fn projected(mut self) -> Self {
        let stale_at = self.created_at.as_millis() + CHATBOT_PENDING_STALE_SECS * 1_000;
        if self.status == MessageStatus::Pending && Timestamp::now().as_millis() > stale_at {
            self.status = MessageStatus::Failed;
            self.error_code = Some(STALE_ERROR_CODE.to_string());
        }
        self
    }

    async fn insert(message: ChatbotMessage, db: &Database) -> Result<ChatbotMessage, AppError> {
        let created: Option<ChatbotMessage> =
            db.create(message.id.record()).content(message).await?;
        created.ok_or_else(|| AppError::Internal("failed to create chat message".into()))
    }

    /// Append the user's prompt. Nothing is awaited for it, so it is born
    /// complete.
    pub async fn append_user(
        thread: &ChatbotThreadId,
        user: &UserId,
        content: ChatContent,
        db: &Database,
    ) -> Result<ChatbotMessage, AppError> {
        let now = Timestamp::now();
        Self::insert(
            ChatbotMessage {
                id: ChatbotMessageId::generate(),
                thread_id: thread.clone(),
                user_id: user.clone(),
                role: MessageRole::User,
                content,
                status: MessageStatus::Complete,
                truncated: false,
                error_code: None,
                created_at: now,
                completed_at: Some(now),
                claimed_by: None,
                claimed_at: None,
            },
            db,
        )
        .await
    }

    /// Reserve the assistant's answer *before* the AI call: the row exists,
    /// empty and `pending`, so a reload finds the turn and can wait on it.
    pub async fn append_pending_assistant(
        thread: &ChatbotThreadId,
        user: &UserId,
        db: &Database,
    ) -> Result<ChatbotMessage, AppError> {
        Self::insert(
            ChatbotMessage {
                id: ChatbotMessageId::generate(),
                thread_id: thread.clone(),
                user_id: user.clone(),
                role: MessageRole::Assistant,
                content: ChatContent::empty(),
                status: MessageStatus::Pending,
                truncated: false,
                error_code: None,
                created_at: Timestamp::now(),
                completed_at: None,
                claimed_by: None,
                claimed_at: None,
            },
            db,
        )
        .await
    }

    /// The whole thread, oldest first — the order both the UI and the AI
    /// history payload read in.
    pub async fn list_for_thread(
        thread: &ChatbotThreadId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ChatbotMessage>, i64), AppError> {
        let (rows, total) = PagedList::new(
            "chatbot_message WHERE thread_id = $conv",
            "ORDER BY created_at ASC, id ASC",
        )
        .bind("conv", thread.record())
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
        thread: &ChatbotThreadId,
        limit: usize,
        db: &Database,
    ) -> Result<Vec<ChatbotMessage>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM chatbot_message WHERE thread_id = $conv \
                 ORDER BY created_at DESC, id DESC LIMIT $limit",
            )
            .bind(("conv", thread.record()))
            .bind(("limit", limit as i64))
            .await?
            .check()?;
        let mut messages: Vec<ChatbotMessage> = result
            .take::<Vec<ChatbotMessage>>(0)?
            .into_iter()
            .map(ChatbotMessage::projected)
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
        thread: &ChatbotThreadId,
        limit: usize,
        db: &Database,
    ) -> Result<Vec<ChatbotMessage>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM chatbot_message WHERE thread_id = $conv \
                 AND status = 'complete' AND content != '' \
                 ORDER BY created_at DESC, id DESC LIMIT $limit",
            )
            .bind(("conv", thread.record()))
            .bind(("limit", limit as i64))
            .await?
            .check()?;
        let mut messages: Vec<ChatbotMessage> = result.take(0)?;
        messages.reverse();
        Ok(messages)
    }

    /// Take up to [`CHATBOT_CLAIM_BATCH`] unanswered turns for the claimant
    /// `me`, and hand back the rows now owed an answer by *this* process.
    ///
    /// The queue read. It is deliberately blind to which capabilities exist
    /// anywhere: the caller has already asked its own in-process registry
    /// whether it can answer, and that registry — never the `ai_worker` gate —
    /// is what makes a claim honest. Claiming a turn this process cannot
    /// dispatch would park it for a whole reclaim horizon.
    ///
    /// A lost race is not an error: the guard is re-evaluated per record inside
    /// the `UPDATE`, so a row a peer took simply does not come back. A
    /// transaction conflict is the same verdict one layer down and yields an
    /// empty batch rather than a failure — the next poll re-reads the queue,
    /// which is also the retry the conflict asks for.
    pub async fn claim_pending(me: &str, db: &Database) -> Result<Vec<ChatbotMessage>, AppError> {
        let result = db
            .query(CHATBOT_CLAIM)
            .bind(("me", me.to_string()))
            .bind(("batch", CHATBOT_CLAIM_BATCH))
            .bind(("stale_ms", CHATBOT_PENDING_STALE_SECS * 1_000))
            .bind(("reclaim_ms", CHATBOT_CLAIM_RECLAIM_SECS * 1_000))
            .await
            .and_then(|response| response.check());
        match result {
            // Statement 0 is the `LET`; the claimed rows are statement 1.
            Ok(mut response) => Ok(response.take::<Vec<ChatbotMessage>>(1)?),
            Err(err) if lost_the_claim(&err) => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    /// The user turn this reserved answer belongs to: the newest `user` row
    /// written before it in the same thread.
    ///
    /// Ordered by `created_at` then `id`, exactly like every other read path —
    /// the two rows one POST writes routinely share a millisecond, so the id
    /// tie-break is what keeps this from picking the prompt of the *previous*
    /// turn (see [`IDS`]).
    pub async fn prompt_for(&self, db: &Database) -> Result<Option<ChatbotMessage>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM chatbot_message WHERE thread_id = $conv AND role = 'user' \
                 AND (created_at < $at OR (created_at = $at AND id < $id)) \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(("conv", self.thread_id.record()))
            .bind(("at", self.created_at.as_millis()))
            .bind(("id", self.id.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ChatbotMessage>>(0)?.into_iter().next())
    }

    /// Read one turn only if `user` owns it — the poll loop's read.
    pub async fn read_for(
        id: &ChatbotMessageId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ChatbotMessage>, AppError> {
        let message: Option<ChatbotMessage> = db.select(id.record()).await?;
        Ok(message
            .filter(|message| &message.user_id == user)
            .map(ChatbotMessage::projected))
    }

    /// Land the answer, recording whether it had to be clipped to fit the
    /// school's cap. Field-scoped and gated on `status = 'pending'` in the
    /// `WHERE`, so a late reply can't overwrite a row the boot sweep (or a
    /// timeout) already failed, and two answers can't both apply.
    pub async fn complete(
        id: &ChatbotMessageId,
        text: ChatContent,
        truncated: bool,
        db: &Database,
    ) -> Result<ChatbotMessage, AppError> {
        Self::settle(id, Some(text), truncated, None, db).await
    }

    /// Mark the answer failed with a short code (`unavailable`, the service's
    /// own error code, …). Same pending gate as [`ChatbotMessage::complete`].
    pub async fn fail(
        id: &ChatbotMessageId,
        error_code: &str,
        db: &Database,
    ) -> Result<ChatbotMessage, AppError> {
        // A failed turn has no text, so there is nothing that could be clipped.
        Self::settle(id, None, false, Some(error_code), db).await
    }

    async fn settle(
        id: &ChatbotMessageId,
        text: Option<ChatContent>,
        truncated: bool,
        error_code: Option<&str>,
        db: &Database,
    ) -> Result<ChatbotMessage, AppError> {
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
            .take::<Vec<ChatbotMessage>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }
}

/// A concurrent claim that the key-value layer decided against us, rather than
/// a broken query. Read from the *typed* surface: every engine's conflict
/// wording funnels into `QueryError::TransactionConflict` (surrealdb-core
/// 3.2.3 `err/to_types.rs:295`), where a substring match covers only whichever
/// wording the engine of the day happens to use.
///
/// Deliberately a second copy of `database::lost_the_race` rather than a shared
/// helper: that one is private to the boot election, which is owned elsewhere
/// and must not gain a caller. Five lines is the cheaper coupling.
fn lost_the_claim(err: &surrealdb::Error) -> bool {
    use surrealdb::types::{ErrorDetails, QueryError};
    matches!(
        err.details(),
        ErrorDetails::Query(Some(QueryError::TransactionConflict))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Value;

    #[tokio::test]
    async fn content_is_required_and_capped() {
        assert!(ChatContent::try_new("").is_err());
        assert!(ChatContent::try_new(&"x".repeat(MAX_CHATBOT_MESSAGE_LEN)).is_ok());
        assert!(ChatContent::try_new(&"x".repeat(MAX_CHATBOT_MESSAGE_LEN + 1)).is_err());
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
        let thread = ChatbotThreadId::from_key("c");
        let user = UserId::from_key("u");
        const TURNS: usize = 200;

        for turn in 0..TURNS {
            let content = ChatContent::try_new(&format!("soru {turn}")).unwrap();
            ChatbotMessage::append_user(&thread, &user, content, &db)
                .await
                .unwrap();
            ChatbotMessage::append_pending_assistant(&thread, &user, &db)
                .await
                .unwrap();
        }

        let (whole, _) = ChatbotMessage::list_for_thread(&thread, None, 0, &db)
            .await
            .unwrap();
        let tail = ChatbotMessage::list_tail(&thread, TURNS * 2, &db)
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

    #[test]
    fn the_claim_horizon_sits_between_the_inference_and_the_stale_window() {
        // Both bounds are load-bearing and neither is decoration:
        // below the AI request timeout, a reclaim fires while the first
        // claimer is still legitimately waiting and two services answer the
        // same turn; at or above the staleness horizon a turn stops being
        // claimable before the reclaim could ever fire, so a dead claimer's
        // turn is never retried at all.
        use crate::constant::AI_DEFAULT_REQUEST_TIMEOUT_SECS;
        assert!(
            CHATBOT_CLAIM_RECLAIM_SECS > AI_DEFAULT_REQUEST_TIMEOUT_SECS as i64,
            "a reclaim inside a legitimate inference double-answers the turn"
        );
        #[allow(clippy::assertions_on_constants)]
        {
            assert!(
                CHATBOT_CLAIM_RECLAIM_SECS < CHATBOT_PENDING_STALE_SECS,
                "a reclaim horizon past the staleness window can never fire"
            );
        }
    }

    #[tokio::test]
    async fn stale_pending_projects_as_failed() {
        let aged = |secs: i64| {
            ChatbotMessage {
                id: ChatbotMessageId::generate(),
                thread_id: ChatbotThreadId::from_key("c"),
                user_id: UserId::from_key("u"),
                role: MessageRole::Assistant,
                content: ChatContent::empty(),
                status: MessageStatus::Pending,
                truncated: false,
                error_code: None,
                created_at: Timestamp::from_millis(Timestamp::now().as_millis() - secs * 1_000),
                completed_at: None,
                claimed_by: None,
                claimed_at: None,
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
