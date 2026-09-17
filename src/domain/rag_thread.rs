//! A RAG thread. It only groups turns and carries the ordering key the
//! list view sorts on: `updated_at` moves on every new turn, so a user's
//! threads list newest-activity-first. The turns themselves live in
//! [`crate::db::rag_message`], and deleting a thread cascades them; the
//! threads' own persistence lives in [`crate::db::rag_thread`].
//!
//! A user's RAG threads are capped per nest but on the **chatbot's knob**:
//! `app_user.rag_thread_count` holds this nest's seats, and the school's
//! `max_chatbot_threads` is the number both nests are read against — the two
//! are one package (`/chatbot` and `/rag` are both `Module::Chatbot`), and the
//! cap exists to bound what one account may store in a school's database, so
//! one knob governs the whole package. Each nest holds its own seats all the
//! same: a user at the cap in one nest may still open a thread in the other,
//! and the counter a claim moves counts exactly the rows it guards.

use crate::constant::MAX_CHATBOT_THREAD_TITLE_LEN;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct RagThreadId(uuid::Uuid);

impl RagThreadId {
    /// Mints from the process-wide monotonic generator, not a plain random
    /// UUID: threads sort `updated_at DESC, id DESC` and the id breaks the tie
    /// between two threads last touched in the same millisecond,
    /// and a random low half sorts arbitrarily inside one millisecond.
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

/// A user-chosen thread name. Required-and-bounded here; "untitled" is
/// `Option<RagThreadTitle>` on the row. Named like the chatbot's
/// ([`crate::domain::chatbot_thread::ChatbotThreadTitle`]) and bounded by the
/// same constant: a name is a line a user typed either way.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct RagThreadTitle(String);

impl RagThreadTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_CHATBOT_THREAD_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RagThread {
    pub(crate) id: RagThreadId,
    /// The owning user. The column is `owner`, not the chatbot's `user_id` —
    /// the RAG nest stores no second participant a name had to disambiguate.
    pub(crate) owner: UserId,
    pub(crate) title: Option<RagThreadTitle>,
    pub(crate) created_at: Timestamp,
    pub(crate) updated_at: Timestamp,
}

impl RagThread {
    pub fn get_id(&self) -> &RagThreadId {
        &self.id
    }

    pub fn get_owner(&self) -> &UserId {
        &self.owner
    }

    pub fn get_title(&self) -> Option<&RagThreadTitle> {
        self.title.as_ref()
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required_and_bounded() {
        assert_eq!(RagThreadTitle::try_new("Fizik").unwrap().as_str(), "Fizik");
        assert!(RagThreadTitle::try_new("   ").is_err());
        assert!(RagThreadTitle::try_new(&"x".repeat(201)).is_err());
    }
}
