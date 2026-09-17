//! Enrollment workflows: the target-user gates the enroll/unenroll HTTP
//! handlers pay (the target must exist and still be a student; unenrolling a
//! pair with no row is a 404) wrapped around the instance-keyed write in
//! [`crate::db::enrollment`], plus the school-scoped club/supervised-study
//! membership ([`crate::db::course_membership`]) with the one rule that keeps
//! the two tiers apart: a class-delivered course is joined through its
//! instance, never directly.

use crate::database::Database;
use crate::db::course_membership;
use crate::db::enrollment;
use crate::domain::class_course::ClassCourseId;
use crate::domain::course::CourseId;
use crate::domain::course_membership::CourseMembership;
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Enroll `target` into one class×course instance on `enrolled_by`'s orders —
/// the hand placement, so the row carries no class `source` and no class sweep
/// can ever take it back. The target-user gates first — they are validation of
/// the request, so a 400, and they run in the order the handler used to: an
/// unknown user outranks a non-student one — then the idempotent write (an
/// already-enrolled pair is returned as-is; 404 when the instance is gone).
pub async fn enroll(
    db: &Database,
    class_course: &ClassCourseId,
    target: &UserId,
    enrolled_by: &UserId,
) -> Result<Enrollment, AppError> {
    ensure_student(db, target, "only students can be enrolled in a course").await?;
    enrollment::enroll(db, class_course, target, enrolled_by, None).await
}

/// Unenroll `target` from one instance. `NotFound` when the pair holds no row
/// — unenrolling someone who was never enrolled removed nothing, so the web
/// layer must not pretend they did.
pub async fn unenroll(
    db: &Database,
    class_course: &ClassCourseId,
    target: &UserId,
) -> Result<(), AppError> {
    enrollment::remove(db, class_course, target)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

pub async fn read_for_user(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    enrollment::read_for_user(db, class_course, user).await
}

/// One instance's roster, newest first, paged.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Enrollment>, i64), AppError> {
    enrollment::list_for_class_course(db, class_course, limit, offset).await
}

/// Join `target` to a school-scoped course — a club or a supervised study —
/// on `added_by`'s orders.
///
/// The one refusal this tier owns: a class-delivered course
/// ([`CourseKind::is_class_delivered`](crate::domain::course::CourseKind::is_class_delivered))
/// has no school-wide roster to join — its students come from the class
/// sections that teach it — so a direct join is a `400` naming the course, and
/// the instance's enrollment route is where that request belongs.
pub async fn join_activity(
    db: &Database,
    course: &CourseId,
    target: &UserId,
    added_by: &UserId,
) -> Result<CourseMembership, AppError> {
    let Some(course_row) = crate::service::course::read(db, course).await? else {
        return Err(AppError::NotFound);
    };
    if course_row.get_kind().is_class_delivered() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "course_id",
            reason: "a ders is joined through the class instance that teaches it, not directly",
        }));
    }
    ensure_student(db, target, "only students can join a course").await?;
    course_membership::add(db, course, target, added_by).await
}

/// Leave a school-scoped course. `NotFound` when the pair holds no membership
/// — leaving something one never joined removed nothing.
pub async fn leave_activity(
    db: &Database,
    course: &CourseId,
    target: &UserId,
) -> Result<(), AppError> {
    course_membership::remove(db, course, target)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

/// The target-user gates both tiers share: the row must exist and be a
/// student. Membership is student membership — it gates sitting exams, being
/// graded, and appearing on a roster — all student-only. Staff run courses,
/// they don't enroll in them.
async fn ensure_student(
    db: &Database,
    target: &UserId,
    reason: &'static str,
) -> Result<(), AppError> {
    let Some(target_user) = crate::db::user::read(db, target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason,
        }));
    }
    Ok(())
}
