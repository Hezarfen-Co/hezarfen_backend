use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{ENROLLMENT_COUNT_FIELD, ENROLLMENT_TABLE};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::class_group::ClassGroupId;
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
///
/// `source` names the class ([`crate::domain::class_group::ClassGroup`]) that
/// pumped this row, and is absent when a human placed the student directly —
/// every row written before classes existed, and every row placed by hand since.
/// Absence *is* the meaning, so nothing backfills it: a class sweep may only take
/// back the rows it wrote.
#[derive(Debug, Clone, SurrealValue)]
pub struct Enrollment {
    id: EnrollmentId,
    course: CourseId,
    user: UserId,
    enrolled_by: UserId,
    source: Option<ClassGroupId>,
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

    /// The class that pumped this row, or `None` for a hand-placed one.
    pub fn get_source(&self) -> Option<&ClassGroupId> {
        self.source.as_ref()
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
            // Hand-placed: no source key at all is written, which is what makes
            // a class sweep unable to take this row back.
            source: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The database strips `NONE`-valued optional columns, and every enrollment
    /// row written before classes existed has no `source` key at all — both must
    /// decode as a hand-placed row, never as a decode error that 500s a roster.
    #[tokio::test]
    async fn enrollment_decodes_without_source_key() {
        use surrealdb::types::Value;

        let class = crate::domain::class_group::ClassGroupId::from_key("9a");
        let course = CourseId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T");
        let student = UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8U");
        let enrollment = Enrollment {
            id: EnrollmentId::composite(&course, &student),
            course,
            user: student,
            enrolled_by: UserId::from_key("mgr"),
            source: Some(class.clone()),
        };
        assert_eq!(enrollment.get_source(), Some(&class));

        let Value::Object(mut object) = enrollment.into_value() else {
            panic!("an enrollment must encode as an object");
        };
        object.remove("source");
        let decoded = Enrollment::from_value(Value::Object(object)).unwrap();
        assert_eq!(decoded.get_source(), None);
    }

    /// And the other half of "absence is the meaning": a hand-placed enrollment
    /// must store *no* source key, or a class sweep would take back a row it
    /// never wrote. Asserted on the stored row, not the returned one.
    #[tokio::test]
    async fn a_hand_placed_enrollment_stores_no_source_key() {
        use crate::domain::course::{Course, CourseDescription, CourseKind, CourseTitle};

        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("teacher");
        let course = Course::create(
            &teacher,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            None,
            None,
            &db,
        )
        .await
        .unwrap();
        Enrollment::enroll(course.get_id(), &UserId::from_key("student"), &teacher, &db)
            .await
            .unwrap();

        let mut result = db
            .query("SELECT VALUE 'source' IN object::keys($this) FROM enrollment")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(
            result.take::<Vec<bool>>(0).unwrap(),
            vec![false],
            "a hand-placed row may carry no source key"
        );
    }
}
