//! Enrollment workflows: the target-user gates the enroll/unenroll HTTP
//! handlers pay (the target must exist and still be a student; unenrolling a
//! pair with no row is a 404) wrapped around the atomic seat claim in
//! [`crate::db::enrollment`].

use crate::database::Database;
use crate::db::enrollment;
use crate::domain::course::CourseId;
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Enroll `target` into `course` on `enrolled_by`'s orders. The target-user
/// gates first — they are validation of the request, so a 400, and they run
/// in the order the handler used to: an unknown user outranks a non-student
/// one — then the claim (idempotent for an already-enrolled pair; 409 on a
/// full roster; 404 when the course is gone).
pub async fn enroll(
    db: &Database,
    course: &CourseId,
    target: &UserId,
    enrolled_by: &UserId,
) -> Result<Enrollment, AppError> {
    let Some(target_user) = User::read(target, db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    // Enrollment is student membership: it gates sitting exams, being graded,
    // and appearing on a lesson roster — all student-only. Staff run courses,
    // they don't enroll in them.
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be enrolled in a course",
        }));
    }

    enrollment::enroll(db, course, target, enrolled_by).await
}

/// Unenroll `target` from `course`. `NotFound` when the pair holds no row —
/// unenrolling someone who was never enrolled removed nothing, so the web
/// layer must not pretend they did.
pub async fn unenroll(db: &Database, course: &CourseId, target: &UserId) -> Result<(), AppError> {
    enrollment::remove(db, course, target)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

pub async fn read_for_user(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    enrollment::read_for_user(db, course, user).await
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Enrollment>, i64), AppError> {
    enrollment::list_for_course(db, course, limit, offset).await
}
