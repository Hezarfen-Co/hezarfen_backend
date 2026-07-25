use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::MAX_SESSION_TOPIC_LEN;
use crate::database::{COURSE_SESSION_TABLE, Database};
use crate::domain::course::CourseId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseSessionId(RecordId);

impl CourseSessionId {
    pub fn generate() -> Self {
        Self(RecordId::new(COURSE_SESSION_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(COURSE_SESSION_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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
#[derive(Debug, Clone, SurrealValue)]
pub struct CourseSession {
    id: CourseSessionId,
    course: CourseId,
    /// Who teaches this session. Must hold the `teacher` role or higher —
    /// enforced at the web layer, where the target user row is at hand.
    teacher: UserId,
    topic: SessionTopic,
    starts_at: Timestamp,
    ends_at: Option<Timestamp>,
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

    pub async fn create(
        course: &CourseId,
        teacher: &UserId,
        topic: SessionTopic,
        starts_at: Timestamp,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<CourseSession, AppError> {
        let session = CourseSession {
            id: CourseSessionId::generate(),
            course: course.clone(),
            teacher: teacher.clone(),
            topic,
            starts_at,
            ends_at,
        };
        let created: Option<CourseSession> =
            db.create(session.id.record()).content(session).await?;
        created.ok_or_else(|| AppError::Internal("failed to create session".into()))
    }

    pub async fn read(
        id: &CourseSessionId,
        db: &Database,
    ) -> Result<Option<CourseSession>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// A course's sessions, most recent lesson first. Ordered by `starts_at`
    /// (not id): a timetable is read by when the lesson happens, not by when
    /// the row was created.
    pub async fn list_for_course(
        course: &CourseId,
        db: &Database,
    ) -> Result<Vec<CourseSession>, AppError> {
        let mut result = db
            .query("SELECT * FROM course_session WHERE course = $course ORDER BY starts_at DESC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<CourseSession>>(0)?)
    }

    /// Field-scoped, like every other save in this codebase: the handler holds
    /// no lock across its read and this write, so a whole-row save would carry
    /// the whole stale row back over anything that landed in between.
    pub async fn update(
        self,
        teacher: UserId,
        topic: SessionTopic,
        starts_at: Timestamp,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<CourseSession, AppError> {
        let mut result = db
            .query(
                "UPDATE $id SET teacher = $teacher, topic = $topic,
                 starts_at = $starts_at, ends_at = $ends_at RETURN AFTER",
            )
            .bind(("id", self.id.record()))
            .bind(("teacher", teacher.record()))
            .bind(("topic", topic))
            .bind(("starts_at", starts_at))
            .bind(("ends_at", ends_at))
            .await?
            .check()?;
        result
            .take::<Vec<CourseSession>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Delete the session and cascade-remove its roll-call rows.
    pub async fn delete(self, db: &Database) -> Result<CourseSession, AppError> {
        db.query("DELETE session_attendance WHERE session = $s")
            .bind(("s", self.id.record()))
            .await?
            .check()?;
        let deleted: Option<CourseSession> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
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
