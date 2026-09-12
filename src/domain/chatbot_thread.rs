//! A chatbot thread. It only groups turns and carries the ordering key the
//! list view sorts on: `updated_at` moves on every new turn, so a user's
//! threads list newest-activity-first. The turns themselves live in
//! [`crate::db::chatbot_message`], and deleting a thread cascades them; the
//! threads' own persistence lives in [`crate::db::chatbot_thread`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{CHATBOT_THREAD_TABLE, MAX_CHATBOT_THREAD_TITLE_LEN};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChatbotThreadId(RecordId);

impl ChatbotThreadId {
    /// Minted from the process-wide monotonic generator, not `Ulid::generate()`:
    /// threads sort `updated_at DESC, id DESC` and the id breaks the tie
    /// between two threads last touched in the same millisecond,
    /// and a random low half sorts arbitrarily inside one millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(CHATBOT_THREAD_TABLE, next_ulid().to_string()))
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
pub struct ChatbotThreadTitle(pub(crate) String);

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
