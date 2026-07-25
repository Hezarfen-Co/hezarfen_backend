//! A chatbot thread. It only groups turns and carries the ordering key the
//! list view sorts on: `updated_at` moves on every new turn, so a user's
//! threads list newest-activity-first. The turns themselves live in
//! [`crate::domain::chatbot_message`], and deleting a thread cascades them.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::database::{CHATBOT_THREAD_TABLE, Database};
use crate::domain::settings::Settings;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Serializes thread creation so the `max_chatbot_threads` check (count,
/// then write) can't over-admit under concurrency — the database's optimistic
/// transactions don't serialize cross-record counts against concurrent
/// inserts. Same reasoning, and the same shape, as `ENROLL_LOCK`.
static CHATBOT_THREAD_LOCK: Mutex<()> = Mutex::const_new(());

/// Bound on a thread's display name. Local to this module because
/// `constant.rs` is another task's file; move it there with the rest of the
/// chat limits when they are next touched.
const MAX_CHATBOT_THREAD_TITLE_LEN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatbotThreadId(RecordId);

impl ChatbotThreadId {
    pub fn generate() -> Self {
        Self(RecordId::new(CHATBOT_THREAD_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(CHATBOT_THREAD_TABLE, key))
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

/// A user-chosen thread name. Required-and-bounded here; "untitled" is
/// `Option<ChatbotThreadTitle>` on the row.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatbotThreadTitle(String);

impl ChatbotThreadTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_CHATBOT_THREAD_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct ChatbotThread {
    id: ChatbotThreadId,
    user_id: UserId,
    title: Option<ChatbotThreadTitle>,
    created_at: Timestamp,
    updated_at: Timestamp,
}

impl ChatbotThread {
    pub fn get_id(&self) -> &ChatbotThreadId {
        &self.id
    }

    pub fn get_user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn get_title(&self) -> Option<&ChatbotThreadTitle> {
        self.title.as_ref()
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }

    pub async fn create(
        user: &UserId,
        title: Option<ChatbotThreadTitle>,
        db: &Database,
    ) -> Result<ChatbotThread, AppError> {
        let now = Timestamp::now();
        let thread = ChatbotThread {
            id: ChatbotThreadId::generate(),
            user_id: user.clone(),
            title,
            created_at: now,
            updated_at: now,
        };
        let created: Option<ChatbotThread> = db.create(thread.id.record()).content(thread).await?;
        created.ok_or_else(|| AppError::Internal("failed to create thread".into()))
    }

    /// Start a thread unless `user` is already at the school's
    /// `max_chatbot_threads`. The whole check-then-write runs under
    /// [`CHATBOT_THREAD_LOCK`], and the cap is re-read inside it, since neither
    /// the transaction model nor a caller-supplied number survives a concurrent
    /// settings PATCH.
    pub async fn create_capped(
        user: &UserId,
        title: Option<ChatbotThreadTitle>,
        db: &Database,
    ) -> Result<ChatbotThread, AppError> {
        let _guard = CHATBOT_THREAD_LOCK.lock().await;
        let cap = Settings::load(db).await?.get_max_chatbot_threads();
        if Self::count_for_user(user, db).await? as i64 >= cap {
            return Err(AppError::Conflict(
                "you have reached the school's limit on saved threads — delete one first",
            ));
        }
        Self::create(user, title, db).await
    }

    /// A user's threads, most recently active first — the sort the
    /// `chatbot_thread_user_updated` index exists for.
    pub async fn list_for_user(
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<ChatbotThread>, AppError> {
        let mut result = db
            .query("SELECT * FROM chatbot_thread WHERE user_id = $usr ORDER BY updated_at DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ChatbotThread>>(0)?)
    }

    /// How many threads `user` keeps — the `max_chatbot_threads` cap check.
    pub async fn count_for_user(user: &UserId, db: &Database) -> Result<usize, AppError> {
        let mut result = db
            .query("SELECT VALUE count() FROM chatbot_thread WHERE user_id = $usr GROUP ALL")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<i64>>(0)?
            .first()
            .copied()
            .unwrap_or_default()
            .max(0) as usize)
    }

    /// Read a thread only if `user` owns it — a foreign id reads as absent, so
    /// the web layer answers 404 rather than leaking that it exists.
    pub async fn read_for(
        id: &ChatbotThreadId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ChatbotThread>, AppError> {
        let thread: Option<ChatbotThread> = db.select(id.record()).await?;
        Ok(thread.filter(|thread| &thread.user_id == user))
    }

    /// Stamp new activity. Field-scoped: a rename may be landing concurrently,
    /// and a whole-row save would clobber it.
    pub async fn touch(&self, db: &Database) -> Result<ChatbotThread, AppError> {
        let mut result = db
            .query("UPDATE $id SET updated_at = $now RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("now", Timestamp::now().as_millis()))
            .await?
            .check()?;
        result
            .take::<Vec<ChatbotThread>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Rename the thread (`None` clears the name back to untitled), stamping
    /// the edit as activity. Field-scoped for the same reason as
    /// [`ChatbotThread::touch`]: a turn may be landing concurrently.
    pub async fn rename(
        &self,
        title: Option<ChatbotThreadTitle>,
        db: &Database,
    ) -> Result<ChatbotThread, AppError> {
        let mut result = db
            .query("UPDATE $id SET title = $title, updated_at = $now RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("title", title.map(|title| title.0)))
            .bind(("now", Timestamp::now().as_millis()))
            .await?
            .check()?;
        result
            .take::<Vec<ChatbotThread>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Delete the thread and every turn in it — one transaction, so a crash
    /// can't orphan messages under a vanished thread.
    pub async fn delete(self, db: &Database) -> Result<ChatbotThread, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE chatbot_message WHERE thread_id = $conv;
                 DELETE $conv RETURN BEFORE;
                 COMMIT TRANSACTION;",
            )
            .bind(("conv", self.id.record()))
            .await?
            .check()?;
        // BEGIN is slot 0; the thread's own DELETE is slot 2.
        let deleted: Option<ChatbotThread> =
            result.take::<Vec<ChatbotThread>>(2)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required_and_bounded() {
        assert_eq!(
            ChatbotThreadTitle::try_new("Fizik").unwrap().as_str(),
            "Fizik"
        );
        assert!(ChatbotThreadTitle::try_new("   ").is_err());
        assert!(ChatbotThreadTitle::try_new(&"x".repeat(201)).is_err());
    }
}
