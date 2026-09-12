use crate::constant::{MAX_SESSION_TOPIC_LEN};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct CourseSessionId(uuid::Uuid);

impl CourseSessionId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: a course's sessions sort `starts_at DESC, id DESC` and the id
    /// breaks the tie between two sessions starting at the same instant,
    /// and a random low half sorts arbitrarily inside one millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }

    /// The inner uuid, for binding the id through the runtime-checked
    /// builders ([`crate::db::page::Param`]) whose values are raw uuids.
    pub(crate) fn uuid(&self) -> uuid::Uuid {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct SessionTopic(String);

impl SessionTopic {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("topic", value, MAX_SESSION_TOPIC_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One scheduled lesson of a course: the unit roll call is taken on. Unlike a
/// generic event, a session always has a start instant and belongs to a course,
/// so its roster is the course's enrollment plus the assigned teacher.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CourseSession {
    pub(crate) id: CourseSessionId,
    pub(crate) course: CourseId,
    /// Who teaches this session. Must hold the `teacher` role or higher —
    /// enforced by [`crate::service::course_session::resolve_session_teacher`],
    /// where the target user row is at hand.
    pub(crate) teacher: UserId,
    pub(crate) topic: SessionTopic,
    pub(crate) starts_at: Timestamp,
    pub(crate) ends_at: Option<Timestamp>,
}

impl CourseSession {
    pub fn get_id(&self) -> &CourseSessionId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_teacher(&self) -> &UserId {
        &self.teacher
    }

    pub fn get_topic(&self) -> &SessionTopic {
        &self.topic
    }

    pub fn get_starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Option<Timestamp> {
        self.ends_at
    }

    pub fn is_teacher(&self, user: &UserId) -> bool {
        &self.teacher == user
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn topic_is_optional_but_bounded() {
        assert!(SessionTopic::try_new("").is_ok());
        assert!(SessionTopic::try_new("limits and continuity").is_ok());
        assert!(SessionTopic::try_new(&"x".repeat(MAX_SESSION_TOPIC_LEN)).is_ok());
        assert!(SessionTopic::try_new(&"x".repeat(MAX_SESSION_TOPIC_LEN + 1)).is_err());
    }
}
