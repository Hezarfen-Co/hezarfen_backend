//! A chatbot thread. It only groups turns and carries the ordering key the
//! list view sorts on: `updated_at` moves on every new turn, so a user's
//! threads list newest-activity-first. The turns themselves live in
//! [`crate::db::chatbot_message`], and deleting a thread cascades them; the
//! threads' own persistence lives in [`crate::db::chatbot_thread`].

use uuid::Uuid;
use crate::constant::{MAX_CHATBOT_THREAD_TITLE_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ChatbotThreadId(uuid::Uuid);

impl ChatbotThreadId {
    /// Mints from the process-wide monotonic generator, not a plain random
    /// UUID: threads sort `updated_at DESC, id DESC` and the id breaks the tie
    /// between two threads last touched in the same millisecond,
    /// and a random low half sorts arbitrarily inside one millisecond.
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

/// A user-chosen thread name. Required-and-bounded here; "untitled" is
/// `Option<ChatbotThreadTitle>` on the row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChatbotThread {
    pub(crate) id: ChatbotThreadId,
    pub(crate) user_id: UserId,
    pub(crate) title: Option<ChatbotThreadTitle>,
    pub(crate) created_at: Timestamp,
    pub(crate) updated_at: Timestamp,
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
