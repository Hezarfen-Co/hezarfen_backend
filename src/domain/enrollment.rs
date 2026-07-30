use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{ENROLLMENT_COUNT_FIELD, ENROLLMENT_TABLE};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::course::{Course, CourseId};
use crate::domain::page::PagedList;
use crate::domain::user::UserId;
use crate::error::AppError;

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
    /// keyed by a deterministic composite id, so concurrent enrolls of the same
    /// pair converge on one row instead of racing the unique index into a 500:
    /// the loser of the `CREATE` reads the winner's row back and returns it.
    /// When the course carries a capacity, a full roster refuses new members
    /// (409) — the seat is taken by [`cap::claim`] on the course row, an atomic
    /// single-record conditional write, so the cap holds against concurrent
    /// enrolls and a concurrent capacity PATCH alike. An already enrolled user is
    /// returned as-is even when the roster is full, and never charged a seat.
    /// The same claim is the delete guard's other half: it is refused outright
    /// once the course row is gone, and while it holds a seat the course cannot
    /// be deleted — so no roster row can outlive its course.
    pub async fn enroll(
        course: &CourseId,
        user: &UserId,
        enrolled_by: &UserId,
        db: &Database,
    ) -> Result<Enrollment, AppError> {
        if let Some(existing) = Self::read_for_user(course, user, db).await? {
            return Ok(existing);
        }
        let capacity = Course::read(course, db)
            .await?
            .ok_or(AppError::NotFound)?
            .get_capacity();
        let enrollment = Enrollment {
            id: EnrollmentId::composite(course, user),
            course: course.clone(),
            user: user.clone(),
            enrolled_by: enrolled_by.clone(),
        };
        // CREATE, not UPSERT, and in the seat's own transaction: a duplicate has
        // to be *seen*, or the pair's second writer would keep the seat it
        // claimed for a row that already existed and the counter would drift
        // above the roster forever.
        match cap::claim_and_create(
            &course.record(),
            ENROLLMENT_COUNT_FIELD,
            capacity.unwrap_or(cap::UNLIMITED),
            &enrollment.id.record(),
            &enrollment,
            db,
        )
        .await?
        {
            cap::Claimed::Made(saved) => Ok(saved),
            // Someone placed this pair first; their row is the answer, and no
            // seat was spent finding that out.
            cap::Claimed::Duplicate => Self::read_for_user(course, user, db)
                .await?
                .ok_or_else(|| AppError::Internal("failed to enroll user".into())),
            // Full, or the course was deleted between the read and the claim —
            // the conditional write matches nothing either way, and only this
            // path pays for the read that tells them apart.
            cap::Claimed::Full => match Course::read(course, db).await? {
                Some(_) => Err(AppError::Conflict("the course is full")),
                None => Err(AppError::NotFound),
            },
        }
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
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Enrollment>, i64), AppError> {
        PagedList::new("enrollment WHERE course = $course", "ORDER BY id DESC")
            .bind("course", course.record())
            .run(limit, offset, db)
            .await
    }

    /// Drop every enrollment `user` holds, across all courses. Only students
    /// enroll, so promotion out of `student` calls this to clear the rosters.
    pub async fn delete_for_user(user: &UserId, db: &Database) -> Result<(), AppError> {
        db.query(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE enrollment WHERE user = $usr RETURN BEFORE);
             FOR $row IN ($gone ?? []) {
                 UPDATE $row.course SET enrollment_count = math::max([(enrollment_count ?? 0) - 1, 0]);
             };
             COMMIT TRANSACTION;",
        )
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
        // The seat comes back in the same transaction as the row that held it,
        // so nothing but a lost transaction can drift the counter.
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE enrollment WHERE course = $course AND user = $usr RETURN BEFORE);
                 UPDATE $course SET enrollment_count = math::max([(enrollment_count ?? 0) - array::len($gone), 0]);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("course", course.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Enrollment>>(3)?.into_iter().next())
    }
}
