//! Parent-link decisions: who may be tied to whom (the admin's `link`),
//! and what a link grants. A link row alone is never the grant — every
//! parent-side permission re-derives it against the student's *live* role
//! through [`links_live`], because the demotion sweeps in
//! [`crate::service::user::set_role`] run once over the rows that exist at
//! that moment and a sweep can lose a race with `link`. The row-storage
//! itself is [`crate::db::parent_link`].

use crate::database::Database;
use crate::db::parent_link;
use crate::domain::parent_link::ParentLink;
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Tie (idempotently) `student` to `parent`. The gate is the school-office
/// rule: `{id}` must hold the `parent` role and `user_id` the `student`
/// role. A stale-link race cannot start here — a target demoted between
/// this check and the upsert fails the role gate again on the next call,
/// and the sweeps treat the fresh row like any other.
///
/// Returns the tie plus the two accounts it was judged on, so the caller
/// can render the response without re-reading either.
pub async fn link(
    db: &Database,
    parent: &UserId,
    student: &UserId,
    linked_by: &UserId,
) -> Result<(ParentLink, User, User), AppError> {
    let parent = crate::service::user::read(db, parent)
        .await?
        .ok_or(AppError::NotFound)?;
    if parent.get_role() != Role::Parent {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "id",
            reason: "students can only be tied to a parent account",
        }));
    }
    let Some(student) = crate::service::user::read(db, student).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    if student.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be tied to a parent",
        }));
    }
    let link = parent_link::link(db, parent.get_id(), student.get_id(), linked_by).await?;
    Ok((link, parent, student))
}

/// True iff `parent` holds a *live* link to `student`: the row exists AND
/// the student side still holds the student role. The link row alone is
/// not the grant — like a stale enrollment, a link whose student side
/// changed role must be inert, so the target's live role is re-read here.
/// A missing or non-student target reads as `false`, so a parent never
/// gets an existence oracle.
pub async fn links_live(
    db: &Database,
    parent: &UserId,
    student: &UserId,
) -> Result<bool, AppError> {
    Ok(parent_link::exists(db, parent, student).await?
        && crate::service::user::read(db, student)
            .await?
            .is_some_and(|student| student.get_role() == Role::Student))
}

/// May `caller` read `target`'s per-student reports (marks, attendance,
/// pomodoro)? Teacher+ always may (per-endpoint narrowing is the caller's
/// business); a parent may exactly when a `parent_link` row ties them to the
/// target *and* the target still holds the student role. Everyone else —
/// students included — gets a 403 (self-reads go through the `/me` endpoints).
pub async fn ensure_can_observe(
    caller: &User,
    target: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if caller.get_role().at_least(Role::Teacher) {
        return Ok(());
    }
    if caller.get_role() == Role::Parent && links_live(db, caller.get_id(), target).await? {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "requires teacher role or higher, or a parent link to this student",
    ))
}

/// Every student `parent` observes, newest link first. Rows, not a live
/// filter — callers re-derive each student's live role themselves when the
/// grant matters (the inert-row filter lives with each reader).
pub async fn list_for_parent(db: &Database, parent: &UserId) -> Result<Vec<ParentLink>, AppError> {
    parent_link::list_for_parent(db, parent).await
}

/// Untie a student from a parent — `Some` iff a tie existed.
pub async fn remove(
    db: &Database,
    parent: &UserId,
    student: &UserId,
) -> Result<Option<ParentLink>, AppError> {
    parent_link::remove(db, parent, student).await
}
