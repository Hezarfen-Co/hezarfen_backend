//! The registration workflow: taking a seat on a registration-audience
//! event. The reads and the seat-returning delete are
//! [`crate::db::registration`]; the seat itself is [`cap`]'s
//! claim-on-the-event-row.

use crate::constant::REGISTRATION_COUNT_FIELD;
use crate::database::Database;
use crate::db::cap;
use crate::db::registration;
use crate::domain::event::EventId;
use crate::domain::registration::{Registration, RegistrationId};
use crate::domain::role::Role;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Register (idempotently) `user` onto `event`, refusing when a capacity
/// cap is set and every seat is taken. Someone already listed gets their
/// existing row back untouched — a true no-op that never counts against
/// the cap and never rewrites who placed them. The seat is taken by
/// [`cap::claim_live_and_create`] on the event row — an atomic single-record
/// conditional write that reads `audience.capacity` off that same row as it
/// decides, so neither a racing registration nor a concurrent PATCH
/// *lowering* the capacity can over-admit. Bound as a number instead, the
/// snapshot below would admit every request already in flight when the
/// lower cap landed.
///
/// The seat is claimed on the event row, so the same statement says nothing
/// about the *user* it is for — and a fall to `parent` is the one demotion
/// whose sweep ([`crate::service::user::set_role`]) can miss a seat and
/// leave it unfreeable: `unregister` refuses a non-student target and a
/// parent cannot reach the route at all. So the transaction also claims the
/// holder's own record ([`cap::role_claim`]), which is the key that
/// demotion writes: either the sweep sees this seat, or this write sees the
/// parent and takes it back.
pub async fn register(
    db: &Database,
    event: &EventId,
    user: &UserId,
    registered_by: &UserId,
) -> Result<Registration, AppError> {
    if let Some(existing) = registration::read_for_user(db, event, user).await? {
        return Ok(existing);
    }
    // Read for its refusals only — the audience kind and the closing time.
    // The seat count itself is re-read by the claim.
    crate::db::event::read(db, event)
        .await?
        .ok_or(AppError::NotFound)?
        .registration_capacity()?;
    let registration_row = Registration {
        id: RegistrationId::composite(event, user),
        event: event.clone(),
        user: user.clone(),
        registered_by: registered_by.clone(),
    };
    match cap::claim_live_and_create(
        &event.record(),
        REGISTRATION_COUNT_FIELD,
        // An uncapped registration list stores no `capacity` key at all
        // (SurrealDB drops a `NONE`-valued object key), so the coalesce is
        // what "unlimited" reads as.
        "audience.capacity ?? $num",
        cap::UNLIMITED,
        // Staff hold their own seats, so the bar is not "still a student"
        // but "still someone who can be taken off the list".
        Some((&user.record(), &format!("= '{}'", Role::Parent.as_str()))),
        (&registration_row.id.record(), &registration_row),
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        // A concurrent placement of the same pair got there first: hand its
        // row over, the same no-op the early return above would have made,
        // and with no seat spent either way.
        cap::Claimed::Duplicate => registration::read_for_user(db, event, user)
            .await?
            .ok_or_else(|| AppError::Internal("failed to register user".into())),
        // Full, the event was deleted between the read and the claim, or
        // the holder fell to parent while this ran — the conditional writes
        // match nothing (or throw) either way, and only this path pays for
        // the reads that tell them apart.
        cap::Claimed::Full => match crate::db::event::read(db, event).await? {
            None => Err(AppError::NotFound),
            Some(_) => match crate::db::user::read(db, user).await? {
                Some(held) if held.get_role() == Role::Parent => Err(AppError::Forbidden(
                    "that account was demoted to parent while this request ran — \
                     only students can be registered for",
                )),
                _ => Err(AppError::Conflict("the event is full")),
            },
        },
    }
}

/// Take a user off the list; `Some` iff they held a seat.
pub async fn remove(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Registration>, AppError> {
    registration::remove(db, event, user).await
}
