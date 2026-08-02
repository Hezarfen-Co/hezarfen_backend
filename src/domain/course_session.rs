use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{COURSE_SESSION_TABLE, MAX_SESSION_TOPIC_LEN};
use crate::database::{Database, transaction_with_retry};
use crate::domain::course::CourseId;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseSessionId(RecordId);

impl CourseSessionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a course's sessions sort `starts_at DESC, id DESC` and the id breaks
    /// the tie between two sessions starting at the same instant,
    /// and a random low half sorts arbitrarily inside one millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(COURSE_SESSION_TABLE, next_ulid().to_string()))
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
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<CourseSession>, i64), AppError> {
        PagedList::new(
            "course_session WHERE course = $course",
            "ORDER BY starts_at DESC, id DESC",
        )
        .bind("course", course.record())
        .run(limit, offset, db)
        .await
    }

    /// Request-scoped: the handler holds nothing across its read and this
    /// write — the range check rides in the `WHERE` — so a field the request
    /// omitted (`None`) is not written at all. Re-sending the snapshot's value
    /// instead would revert a concurrent PATCH of that field — scoping the
    /// `SET` alone does not stop that, the values have to come from the
    /// request. `ends_at` is nullable, so it takes the outer/inner
    /// `Option<Option<_>>`: `None` = omitted (keep), `Some(None)` = clear.
    pub async fn update(
        self,
        teacher: Option<UserId>,
        topic: Option<SessionTopic>,
        starts_at: Option<Timestamp>,
        ends_at: Option<Option<Timestamp>>,
        db: &Database,
    ) -> Result<CourseSession, AppError> {
        FieldUpdate::new(self.id.record())
            .set("teacher", teacher.map(|teacher| teacher.record()))
            .set("topic", topic)
            .set("starts_at", starts_at)
            .set("ends_at", ends_at)
            .ordered("starts_at", "ends_at", range_error())
            .run::<CourseSession>(db)
            .await
    }

    /// Delete the session and cascade-remove its roll-call rows, in one
    /// transaction: as two unbatched queries a failure between them stranded
    /// roll-call rows on a session that was already gone. The other half of
    /// that invariant is [`crate::domain::session_attendance::SessionAttendance::mark`],
    /// which proves the session still exists inside its own write — a
    /// transaction here cannot stop a mark that commits *after* this one.
    pub async fn delete(self, db: &Database) -> Result<CourseSession, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 DELETE session_attendance WHERE session = $s;
                 LET $before = (DELETE $s RETURN BEFORE);
                 RETURN $before;
                 COMMIT TRANSACTION;",
            &[("s".into(), self.id.record().into_value())],
            // No THROW of its own — an unconditional cascade, so the only
            // error worth telling apart is a lost round (see
            // [`crate::domain::exam::Exam::delete`]).
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Read through the trailing `RETURN`, never a hand-counted slot.
        let slot = result.num_statements().saturating_sub(2);
        let deleted: Option<CourseSession> =
            result.take::<Vec<CourseSession>>(slot)?.into_iter().next();
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
