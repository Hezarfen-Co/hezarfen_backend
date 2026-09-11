//! Course-session workflows: who may teach a lesson (the teacher-resolution
//! gate), and the doors the web layer takes for every session read and
//! write. The queries live in [`crate::db::course_session`]; the
//! timetable's wire shaping and the course-rights checks stay in the web
//! layer.

use crate::database::Database;
use crate::db::course_session;
use crate::domain::course::CourseId;
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Resolve who a session's teacher should be: the caller when `teacher_id` is
/// omitted (or names them), otherwise the referenced user — who must exist and
/// hold the `teacher` role or higher (a student cannot teach a lesson).
///
/// Scheduling hands the named person the session's teaching seat, so the
/// floor is checked on the *live* role here rather than at the web layer:
/// every future roll-call right derives from this column, and a stale
/// `teacher_id` in a request body must fail with the same 400 no matter
/// which route named it.
pub async fn resolve_session_teacher(
    teacher_id: Option<&str>,
    caller: &User,
    db: &Database,
) -> Result<User, AppError> {
    let target = match teacher_id {
        None => return Ok(caller.clone()),
        Some(key) if key == caller.get_id().key() => return Ok(caller.clone()),
        Some(key) => UserId::from_key(key),
    };
    let Some(user) = crate::db::user::read(db, &target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "session teacher does not exist",
        }));
    };
    if !user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "session teacher must hold the teacher role or higher",
        }));
    }
    Ok(user)
}

/// Schedule a lesson on `course`. The write bumps the course row in the same
/// transaction (`cap::touch_and_create`), so a lesson can never outlive its
/// course — a create racing the course delete refuses instead of orphaning.
pub async fn create(
    db: &Database,
    course: &CourseId,
    teacher: &UserId,
    topic: SessionTopic,
    starts_at: Timestamp,
    ends_at: Option<Timestamp>,
) -> Result<CourseSession, AppError> {
    course_session::create(db, course, teacher, topic, starts_at, ends_at).await
}

pub async fn read(db: &Database, id: &CourseSessionId) -> Result<Option<CourseSession>, AppError> {
    course_session::read(db, id).await
}

/// A course's sessions, most recent lesson first — the timetable read.
pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseSession>, i64), AppError> {
    course_session::list_for_course(db, course, limit, offset).await
}

/// Only what the request carried is written: an omitted field (`None`) is
/// not stored at all, so a concurrent PATCH of that field survives. The
/// range consistency re-check rides in the UPDATE's own `WHERE`, so the
/// caller holds nothing across its read and this write.
pub async fn update(
    db: &Database,
    session: CourseSession,
    teacher: Option<UserId>,
    topic: Option<SessionTopic>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Option<Timestamp>>,
) -> Result<CourseSession, AppError> {
    course_session::update(db, session, teacher, topic, starts_at, ends_at).await
}

/// Delete the session and sweep its roll-call rows in one transaction.
pub async fn delete(db: &Database, session: CourseSession) -> Result<CourseSession, AppError> {
    course_session::delete(db, session).await
}
