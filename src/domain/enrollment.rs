use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;

use crate::database::{Database, ENROLLMENT_TABLE};
use crate::domain::course::{Course, CourseId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Serializes enrolls so the capacity check (count, then write) can't
/// over-admit under concurrency — the database's optimistic transactions
/// don't serialize cross-record counts against concurrent inserts.
static ENROLL_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EnrollmentId(RecordId);

impl EnrollmentId {
    /// A deterministic id for the (course, user) pair. The same pair always
    /// maps to the same record id, so enrolling is a single atomic UPSERT with
    /// no find-then-insert race and one-row-per-pair by construction. ULID keys
    /// are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(course: &CourseId, user: &UserId) -> Self {
        Self(RecordId::new(
            ENROLLMENT_TABLE,
            format!("{}_{}", course.key(), user.key()),
        ))
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

/// A user's membership in a course. Grading requires it; removing it hides the
/// user's marks from the report but never deletes result rows.
#[derive(Debug, Clone, SurrealValue)]
pub struct Enrollment {
    id: EnrollmentId,
    course: CourseId,
    user: UserId,
    enrolled_by: UserId,
}

impl Enrollment {
    pub fn get_id(&self) -> &EnrollmentId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_enrolled_by(&self) -> &UserId {
        &self.enrolled_by
    }

    /// Enroll (idempotently) `user` into `course`. One row per (course, user),
    /// keyed by a deterministic composite id so this is a single atomic UPSERT —
    /// concurrent enrolls for the same pair converge on one row instead of
    /// racing the unique index into a 500. When the course carries a capacity,
    /// a full roster refuses new members (409) — the whole check-then-write
    /// runs under [`ENROLL_LOCK`], and the cap is re-derived from a fresh
    /// course read inside it, since neither the transaction model nor a
    /// caller-supplied course survives a concurrent capacity PATCH. An already
    /// enrolled user is returned as-is even when the roster is full.
    pub async fn enroll(
        course: &CourseId,
        user: &UserId,
        enrolled_by: &UserId,
        db: &Database,
    ) -> Result<Enrollment, AppError> {
        let _guard = ENROLL_LOCK.lock().await;
        if let Some(existing) = Self::read_for_user(course, user, db).await? {
            return Ok(existing);
        }
        let capacity = Course::read(course, db)
            .await?
            .ok_or(AppError::NotFound)?
            .get_capacity();
        if let Some(capacity) = capacity
            && Self::list_for_course(course, db).await?.len() as i64 >= capacity
        {
            return Err(AppError::Conflict("the course is full"));
        }
        let enrollment = Enrollment {
            id: EnrollmentId::composite(course, user),
            course: course.clone(),
            user: user.clone(),
            enrolled_by: enrolled_by.clone(),
        };
        let saved: Option<Enrollment> = db
            .upsert(enrollment.id.record())
            .content(enrollment)
            .await?;
        saved.ok_or_else(|| AppError::Internal("failed to enroll user".into()))
    }

    /// Some(_) iff `user` is enrolled in `course` — the grading gate.
    pub async fn read_for_user(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Enrollment>, AppError> {
        let mut result = db
            .query("SELECT * FROM enrollment WHERE course = $course AND user = $usr LIMIT 1")
            .bind(("course", course.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Enrollment>>(0)?.into_iter().next())
    }

    pub async fn list_for_course(
        course: &CourseId,
        db: &Database,
    ) -> Result<Vec<Enrollment>, AppError> {
        let mut result = db
            .query("SELECT * FROM enrollment WHERE course = $course ORDER BY id DESC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Enrollment>>(0)?)
    }

    /// Drop every enrollment `user` holds, across all courses. Only students
    /// enroll, so promotion out of `student` calls this to clear the rosters.
    pub async fn delete_for_user(user: &UserId, db: &Database) -> Result<(), AppError> {
        db.query("DELETE enrollment WHERE user = $usr")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
    }

    pub async fn remove(
        course: &CourseId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Enrollment>, AppError> {
        let mut result = db
            .query("DELETE enrollment WHERE course = $course AND user = $usr RETURN BEFORE")
            .bind(("course", course.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Enrollment>>(0)?.into_iter().next())
    }
}
