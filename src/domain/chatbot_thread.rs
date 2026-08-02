//! A chatbot thread. It only groups turns and carries the ordering key the
//! list view sorts on: `updated_at` moves on every new turn, so a user's
//! threads list newest-activity-first. The turns themselves live in
//! [`crate::domain::chatbot_message`], and deleting a thread cascades them.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    CHATBOT_THREAD_COUNT_FIELD, CHATBOT_THREAD_TABLE, DEFAULT_MAX_CHATBOT_THREADS,
    MAX_CHATBOT_THREAD_TITLE_LEN, SETTINGS_KEY, SETTINGS_TABLE,
};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

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

    /// Start a thread unless `user` is already at the school's
    /// `max_chatbot_threads`. The slot is taken on the user row in the same
    /// transaction as the thread ([`cap::claim_live_and_create`]) — an atomic
    /// single-record write, so two requests racing the same user's last slot
    /// cannot both win and the counter can never count a row that did not
    /// commit.
    ///
    /// The cap is the one *live* on the settings singleton, sub-queried inside
    /// that same conditional write rather than bound as a number: this cap does
    /// not live on the parent row (the seat is on the user, the limit is the
    /// school's), and a snapshot of it admits every request already in flight
    /// when a `PATCH /settings` lowers it. The subquery costs one extra record
    /// read per claim on a path that already reads the row it writes.
    /// `DEFAULT_MAX_CHATBOT_THREADS` is the fallback the settings row's own
    /// absent column means — no row, or a school that never set the knob.
    pub async fn create_capped(
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
        match cap::claim_live_and_create(
            &user.record(),
            CHATBOT_THREAD_COUNT_FIELD,
            &format!(
                "(SELECT VALUE max_chatbot_threads FROM ONLY {SETTINGS_TABLE}:{SETTINGS_KEY}) ?? $num"
            ),
            DEFAULT_MAX_CHATBOT_THREADS,
            &thread.id.record(),
            &thread,
            db,
        )
        .await?
        {
            cap::Claimed::Made(saved) => Ok(saved),
            // Full, or the user's row is gone — the conditional write matches
            // nothing either way.
            cap::Claimed::Full => Err(AppError::Conflict(
                "you have reached the school's limit on saved threads — delete one first",
            )),
            // The id is a freshly minted ULID on a table with no UNIQUE index,
            // so no rival can have aimed at it.
            cap::Claimed::Duplicate => Err(AppError::Internal("thread id collided".into())),
        }
    }

    /// A user's threads, most recently active first — the sort the
    /// `chatbot_thread_user_updated` index exists for.
    pub async fn list_for_user(
        user: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<ChatbotThread>, i64), AppError> {
        PagedList::new(
            "chatbot_thread WHERE user_id = $usr",
            "ORDER BY updated_at DESC",
        )
        .bind("usr", user.record())
        .run(limit, offset, db)
        .await
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
    /// can't orphan messages under a vanished thread. The owner's slot comes
    /// back in that same transaction, or the cap would ratchet shut.
    pub async fn delete(self, db: &Database) -> Result<ChatbotThread, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE chatbot_message WHERE thread_id = $conv;
                 LET $gone = (DELETE $conv RETURN BEFORE);
                 UPDATE $usr SET chatbot_thread_count = math::max([(chatbot_thread_count ?? 0) - array::len($gone), 0]);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("conv", self.id.record()))
            .bind(("usr", self.user_id.record()))
            .await?
            .check()?;
        let deleted: Option<ChatbotThread> =
            result.take::<Vec<ChatbotThread>>(4)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{Settings, SettingsParams};

    /// The owner's counter and the threads it counts, both re-read out of the
    /// store — never off a return value, which the in-memory engine forges
    /// wins on (see [`cap`]).
    async fn stored(db: &Database) -> (i64, usize) {
        let mut result = db
            .query("SELECT VALUE (chatbot_thread_count ?? 0) FROM user:u")
            .query("SELECT VALUE id FROM chatbot_thread")
            .await
            .unwrap()
            .check()
            .unwrap();
        let counter = result.take::<Vec<i64>>(0).unwrap();
        let rows = result.take::<Vec<RecordId>>(1).unwrap();
        (counter[0], rows.len())
    }

    async fn a_user_capped_at(threads: i64) -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query("CREATE user:u SET username = 'u', password_hash = 'x';")
            .await
            .unwrap()
            .check()
            .unwrap();
        Settings::try_new(SettingsParams {
            max_chatbot_threads: threads,
            ..Settings::defaults().params()
        })
        .unwrap()
        .save(&db)
        .await
        .unwrap();
        db
    }

    /// The seat and the row commit together, so the counter the cap reads can
    /// never disagree with the threads it counts — and a create refused at the
    /// cap advances neither.
    #[tokio::test]
    async fn a_capped_create_moves_the_counter_with_the_row() {
        let db = a_user_capped_at(1).await;
        let user = UserId::from_key("u");

        ChatbotThread::create_capped(&user, None, &db)
            .await
            .expect("first thread");
        assert_eq!(stored(&db).await, (1, 1));

        let refused = ChatbotThread::create_capped(&user, None, &db).await;
        assert!(matches!(refused, Err(AppError::Conflict(_))), "at the cap");
        assert_eq!(
            stored(&db).await,
            (1, 1),
            "a refused create advances neither"
        );
    }

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
