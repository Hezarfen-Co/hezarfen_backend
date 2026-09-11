//! The roll-call workflows: taking and clearing a lesson's roll call, with
//! the target-eligibility rules every roll-call path shares — a staff row is
//! management's to write (the session's own teacher is marked and cleared
//! only by manager+), only students attend lessons, and a non-teacher target
//! must be enrolled in the course. The queries and the badge-counter
//! transactions live in [`crate::db::session_attendance`]; who may take a
//! roll call at all (the session's teacher or a course manager) is the web
//! layer's authorization, judged against the session and course it loaded.

use crate::database::Database;
use crate::db::session_attendance;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::course_session::CourseSession;
use crate::domain::role::Role;
use crate::domain::session_attendance::SessionAttendance;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Record `target`'s roll-call state for the session — the target-eligibility
/// gates plus the mark. Returns the written row and the target's user row as
/// the gates judged it (the response joins both people).
///
/// - The session's own teacher is marked only by manager+: staff presence is
///   management's call, so a teacher can't mark themselves present.
/// - Everyone else on a lesson's roster is an enrolled student. Only students
///   attend classes; checking the live role keeps a stale enrollment (left by
///   a promotion) from putting staff on the roll.
///
/// A badge is a decoration on top of the roll call: losing one to a
/// transient database error must never fail the mark, and the next counter
/// move re-runs this and heals it. Both people the mark can credit are
/// synced — the person marked (`lessons_attended`) and the lesson's teacher
/// (`lessons_held`, on the first roll call only). The teacher's runs on every
/// mark rather than only that first one: `sync` is add-only and idempotent,
/// so the extra calls cost one record read and are what heals a first sync
/// that failed, and knowing here whether the credit landed would mean
/// widening the write's return type for nothing.
pub async fn mark(
    db: &Database,
    session: &CourseSession,
    marker: &User,
    target: &UserId,
    status: AttendanceStatus,
) -> Result<(SessionAttendance, User), AppError> {
    // Target user must exist.
    let target_user = crate::db::user::read(db, target)
        .await?
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }))?;

    if session.is_teacher(target) {
        // The session teacher's own presence is recorded by management.
        if !marker.get_role().at_least(Role::Manager) {
            return Err(AppError::Forbidden(
                "marking the session's teacher requires manager role or higher",
            ));
        }
    } else {
        // Everyone else on a lesson's roster is an enrolled student. Only
        // students attend classes; checking the live role keeps a stale
        // enrollment (left by a promotion) from putting staff on the roll.
        if target_user.get_role() != Role::Student {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "user_id",
                reason: "only students can be marked present in a lesson",
            }));
        }
        if crate::db::enrollment::read_for_user(db, session.get_course(), target)
            .await?
            .is_none()
        {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "user_id",
                reason: "target user is not enrolled in this course",
            }));
        }
    }

    let attendance = session_attendance::mark(db, session, target, status, marker.get_id()).await?;
    for who in [target, session.get_teacher()] {
        if let Err(err) = crate::db::badge::sync(db, who).await {
            tracing::warn!("failed to sync badges for {}: {err}", who.key());
        }
    }
    Ok((attendance, target_user))
}

/// List a session's roll call, paged.
pub async fn list_for_session(
    db: &Database,
    session: &crate::domain::course_session::CourseSessionId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<SessionAttendance>, i64), AppError> {
    session_attendance::list_for_session(db, session, limit, offset).await
}

/// Every roll-call row ever recorded for `user` — the session half of the
/// attendance report.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
) -> Result<Vec<SessionAttendance>, AppError> {
    session_attendance::list_for_user(db, user).await
}

/// Clear `target`'s roll-call row from the session, with the one gate the
/// mark does not have: a staff row is management's to remove, keyed on the
/// *target's live role* rather than on `is_teacher` — reassigning a
/// session's teacher used to hand the incoming teacher, an ordinary one, the
/// outgoing teacher's staff row to delete without the manager+ this guard
/// exists to require. A target whose user row is gone can only be a
/// student's stale row (marking checks the role at write time), so it stays
/// the session teacher's to clear.
///
/// `Err(NotFound)` when there was no row to remove.
pub async fn remove(
    db: &Database,
    session: &CourseSession,
    remover: &User,
    target: &UserId,
) -> Result<SessionAttendance, AppError> {
    let staff_row = crate::db::user::read(db, target)
        .await?
        .is_some_and(|target_user| target_user.get_role().at_least(Role::Teacher));
    if staff_row && !remover.get_role().at_least(Role::Manager) {
        return Err(AppError::Forbidden(
            "removing a staff roll-call row requires manager role or higher",
        ));
    }
    session_attendance::remove(db, session.get_id(), target)
        .await?
        .ok_or(AppError::NotFound)
}
