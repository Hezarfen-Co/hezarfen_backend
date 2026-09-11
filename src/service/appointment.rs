//! Appointment workflows: booking, the approve/reject/cancel decisions, and
//! counter-proposals. Reads, listings, the overlap probe, and the
//! compare-and-set live in [`crate::db::appointment`]; the slot's seat is
//! [`crate::db::cap`]'s claim.
//!
//! The overlap guarantee is [`APPOINTMENT_LOCK`]: booking and approval hold
//! it across "is this person already committed at that time" and the write
//! that commits — a predicate over *other* rows that no single-record
//! conditional write can decide.

use tokio::sync::Mutex;

use crate::constant::{CAS_UPDATE_RETRIES, SLOT_OCCUPIED_FIELD};
use crate::database::Database;
use crate::db::appointment;
use crate::db::appointment_slot;
use crate::db::cap;
use crate::domain::appointment::{
    Appointment, AppointmentId, AppointmentReason, AppointmentStatus,
};
use crate::domain::appointment_slot::AppointmentSlotId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Serializes the **overlap** check and the write it authorizes — nothing else.
/// "Is this teacher (or this requester) already committed at that time" is a
/// predicate over other appointment rows, and SurrealDB does not conflict-check
/// a cross-record read against a concurrent insert, so the check and its write
/// have to be one atomic step, and this lock is the only thing that makes them
/// one. Occupancy is no longer its business: that is the slot's `occupied`
/// counter, a conditional single-record write the store decides.
///
/// So this is load-bearing, not a convenience: an approval path that does not
/// take it can double-book a teacher, as the domain's module doc records.
///
/// Lock order: taken *before* `cap`'s `CLAIM_LOCK` (booking claims a slot while
/// holding this) and never the other way round. No path ever holds this
/// together with `ENROLL_LOCK`,
/// `TERM_LOCK`, `EXAM_LOCK`, or `PRESENCE_LOCK` — appointments
/// touch no roster, term, event seat, or exam room, and none of those touch
/// appointments — so they cannot deadlock. Should a future path ever need two,
/// take this one *last*.
// corner-cut: global lock; per-teacher locks if appointment traffic ever grows
// enough for the contention to matter.
pub(crate) static APPOINTMENT_LOCK: Mutex<()> = Mutex::const_new(());

/// Request `slot`. Lands `pending`: publishing availability is not consent
/// to a specific person and topic.
///
/// Refused (409) when the slot's window has already opened, when it is
/// already taken, or when the requester is already committed elsewhere at
/// that time. The slot is taken by [`cap::claim_and_create`], which writes
/// the booking in the *same transaction* as the seat — a conditional
/// single-record write, so two racing requests cannot both find it free
/// however they interleave, and a crash between the two can no longer leave
/// the slot occupied by a booking that does not exist. The requester's own
/// overlap is the lock's part.
pub async fn book(
    db: &Database,
    slot: &AppointmentSlotId,
    requester: &UserId,
    reason: AppointmentReason,
) -> Result<Appointment, AppError> {
    let _guard = APPOINTMENT_LOCK.lock().await;
    let slot_row = appointment_slot::read(db, slot)
        .await?
        .ok_or(AppError::NotFound)?;
    // A window that has opened is history, not a plan: `cancel` refuses to
    // call off a meeting that started, so such a booking would be stuck
    // forever. `list_upcoming` is bounded on this same `starts_at`, so the
    // calendar never offers one — but a slot whose start passed while the
    // page was open, or an id kept from an earlier read, still reaches here.
    // The publish-time 60s skew grace does not apply on either side.
    if slot_row.get_starts_at().as_millis() <= Timestamp::now().as_millis() {
        return Err(AppError::Conflict("the slot has already started"));
    }
    // The teacher's own overlaps are not checked here — an approved
    // meeting of theirs elsewhere is the *approval*'s business, and
    // refusing the request outright would hide a slot they published.
    if appointment::conflicts(
        db,
        requester,
        requester,
        slot_row.get_starts_at(),
        slot_row.get_ends_at(),
        None,
    )
    .await?
    {
        return Err(AppError::Conflict(
            "you already have an appointment at that time",
        ));
    }
    // Last, so nothing above can refuse *after* the slot was taken. A slot
    // deleted since the read above is gone, not full — the claim's `WHERE`
    // finds no row either way, so the distinction is re-read rather than
    // guessed.
    let seat = slot.record();
    let appointment_row = Appointment {
        id: AppointmentId::generate(),
        slot: slot.clone(),
        requester: requester.clone(),
        status: AppointmentStatus::Pending,
        reason,
        proposed_starts_at: None,
        proposed_ends_at: None,
        proposed_by: None,
        decided_by: None,
        cancelled_by: None,
        cancel_reason: None,
        reject_reason: None,
        created_at: Timestamp::now(),
    };
    match cap::claim_and_create(
        &seat,
        SLOT_OCCUPIED_FIELD,
        1,
        &appointment_row.id.record(),
        &appointment_row,
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        cap::Claimed::Full => {
            if appointment_slot::read(db, slot).await?.is_none() {
                return Err(AppError::NotFound);
            }
            Err(AppError::Conflict("the slot is already booked"))
        }
        // The id is a freshly minted ULID on a table with no UNIQUE index,
        // so no rival can have aimed at it.
        cap::Claimed::Duplicate => Err(AppError::Internal("appointment id collided".into())),
    }
}

/// The row, for callers that only inspect it — the web layer's authorization
/// gates read through here.
pub async fn read(db: &Database, id: &AppointmentId) -> Result<Option<Appointment>, AppError> {
    appointment::read(db, id).await
}

/// The requester's bookings, newest first — the web layer's paging read.
pub async fn list_for_requester(
    db: &Database,
    requester: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Appointment>, i64), AppError> {
    appointment::list_for_requester(db, requester, limit, offset).await
}

/// Every booking aimed at `teacher`, newest first — the request inbox.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Appointment>, i64), AppError> {
    appointment::list_for_teacher(db, teacher, limit, offset).await
}

/// Confirm the meeting at the slot's own window. Re-runs the double-booking
/// guard for both teacher and requester against a fresh read under
/// [`APPOINTMENT_LOCK`] — the authoritative check, since only approval
/// commits anyone. Refused (409) when the effective window has already
/// started; that also covers [`accept_proposal`], which lands here.
///
/// Also refused (409) while a counter-proposal stands: the effective window
/// would then be the *proposed* one, and the deciding side here is the
/// teacher's, so approving would commit the requester to a time only the
/// teacher ever named. That move belongs to [`accept_proposal`], which the
/// web layer opens to the requester alone.
pub async fn approve(
    db: &Database,
    id: &AppointmentId,
    decided_by: &UserId,
) -> Result<Appointment, AppError> {
    approve_inner(db, id, decided_by, None).await
}

/// The one approval path. `accepting` carries the window the caller is
/// answering *for*, which is the only way a proposed one may be committed:
/// it both marks the caller as the proposal's counterparty and pins which
/// proposal they saw. Re-checked on every retry round, so a proposal that
/// moved between the read and the write is refused rather than confirmed.
async fn approve_inner(
    db: &Database,
    id: &AppointmentId,
    decided_by: &UserId,
    accepting: Option<(Timestamp, Timestamp)>,
) -> Result<Appointment, AppError> {
    let _guard = APPOINTMENT_LOCK.lock().await;
    for _ in 0..CAS_UPDATE_RETRIES {
        let expected = appointment::read_pending(db, id).await?;
        match accepting {
            None if expected.has_proposal() => {
                return Err(AppError::Conflict(
                    "a counter-proposal is standing; only the requester can accept it",
                ));
            }
            Some(_) if !expected.has_proposal() => {
                return Err(AppError::Conflict("no time has been proposed"));
            }
            // The proposal moved between the requester reading it and
            // accepting it. Committing here would bind them to a window the
            // teacher swapped in after they clicked, which is the whole
            // thing the requester-only accept exists to prevent.
            Some(accepted) if !expected.proposes(accepted) => {
                return Err(AppError::Conflict(
                    "the proposed time has changed; re-read the booking and accept the new one",
                ));
            }
            _ => {}
        }
        let slot = appointment_slot::read(db, &expected.slot)
            .await?
            .ok_or(AppError::NotFound)?;
        let (starts_at, ends_at) = expected.window(&slot);
        // The effective window — the proposal's when one stands — must still
        // be ahead. Mutual agreement does not help: `cancel` refuses a
        // meeting that started, so committing to a passed window would mint
        // a booking nobody can ever undo. Refused, the row stays `pending`
        // and the teacher can propose a time that can actually happen.
        if starts_at.as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("that time has already started"));
        }
        if appointment::conflicts(
            db,
            slot.get_teacher(),
            &expected.requester,
            starts_at,
            ends_at,
            Some(id),
        )
        .await?
        {
            return Err(AppError::Conflict(
                "that time collides with another approved appointment",
            ));
        }
        let mut approved = expected.clone();
        approved.status = AppointmentStatus::Approved;
        approved.decided_by = Some(decided_by.clone());
        if let Some(saved) = appointment::save_if_unchanged(db, &expected, approved).await? {
            return Ok(saved);
        }
    }
    Err(contended())
}

/// Turn the request down. Frees the slot — the seat goes back with the
/// status flip, in one transaction. No lock: this decides nothing about
/// anyone's calendar, and the write is its own guard.
pub async fn reject(
    db: &Database,
    id: &AppointmentId,
    decided_by: &UserId,
    reason: Option<AppointmentReason>,
) -> Result<Appointment, AppError> {
    for _ in 0..CAS_UPDATE_RETRIES {
        let expected = appointment::read_pending(db, id).await?;
        let mut rejected = expected.clone();
        rejected.status = AppointmentStatus::Rejected;
        rejected.decided_by = Some(decided_by.clone());
        rejected.reject_reason = reason.clone();
        if let Some(saved) = appointment::save_if_unchanged(db, &expected, rejected).await? {
            return Ok(saved);
        }
    }
    Err(contended())
}

/// Call the meeting off. Legal from either live state, so an approved
/// meeting can still be dropped; the slot frees up. The web layer opens
/// this to the **requester only** — a teacher's way out is `reject` while
/// the booking is pending, or `reschedule` (back to `pending`) then
/// `reject` for an approved one — and `decline_reschedule`, the other
/// caller, is the requester's too.
///
/// Refused (409) once the *effective* window has opened — a meeting that
/// began is history, not a plan. The guard lives here rather than in the
/// handler so every caller inherits it: `decline_reschedule` cancels
/// through this same door and used to slip past a handler-only check.
pub async fn cancel(
    db: &Database,
    id: &AppointmentId,
    cancelled_by: &UserId,
    reason: Option<AppointmentReason>,
) -> Result<Appointment, AppError> {
    for _ in 0..CAS_UPDATE_RETRIES {
        let expected = appointment::read(db, id).await?.ok_or(AppError::NotFound)?;
        if !expected.status.is_live() {
            return Err(AppError::Conflict("the appointment is already settled"));
        }
        let slot = appointment_slot::read(db, &expected.slot)
            .await?
            .ok_or(AppError::NotFound)?;
        if expected.window(&slot).0.as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("the appointment has already started"));
        }
        let mut cancelled = expected.clone();
        cancelled.status = AppointmentStatus::Cancelled;
        cancelled.cancelled_by = Some(cancelled_by.clone());
        cancelled.cancel_reason = reason.clone();
        if let Some(saved) = appointment::save_if_unchanged(db, &expected, cancelled).await? {
            return Ok(saved);
        }
    }
    Err(contended())
}

/// Counter-propose another time on the same booking (no second row, so the
/// reason and the history stay in one place).
///
/// The proposal sends the booking back to `pending`: a previously approved
/// meeting is *not* committed at the new time until the other side accepts,
/// and leaving it approved would silently move a confirmed appointment.
///
/// Refused (409) when the proposed window has already opened. The handler's
/// `check_not_past` carries a 60s clock-skew grace, which is enough to
/// propose a window that is already underway; `cancel` would then refuse to
/// undo the booking, so it must not be offered in the first place.
pub async fn propose(
    db: &Database,
    id: &AppointmentId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    proposed_by: &UserId,
) -> Result<Appointment, AppError> {
    if starts_at.as_millis() >= ends_at.as_millis() {
        return Err(ValidationError::Invalid {
            field: "proposed_ends_at",
            reason: "must be after proposed_starts_at",
        }
        .into());
    }
    // No lock: a proposal commits nobody's calendar (the booking goes back
    // to `pending`), and it neither takes nor frees the slot's seat.
    for _ in 0..CAS_UPDATE_RETRIES {
        let expected = appointment::read(db, id).await?.ok_or(AppError::NotFound)?;
        if !expected.status.is_live() {
            return Err(AppError::Conflict("the appointment is already settled"));
        }
        if starts_at.as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("that time has already started"));
        }
        let mut proposed = expected.clone();
        proposed.status = AppointmentStatus::Pending;
        proposed.decided_by = None;
        proposed.proposed_starts_at = Some(starts_at);
        proposed.proposed_ends_at = Some(ends_at);
        proposed.proposed_by = Some(proposed_by.clone());
        if let Some(saved) = appointment::save_if_unchanged(db, &expected, proposed).await? {
            return Ok(saved);
        }
    }
    Err(contended())
}

/// Accept the standing proposal — approval at the proposed time. The
/// overlap guard runs again because the time moved.
///
/// `starts_at`/`ends_at` are the proposal the caller is answering, as they
/// read it. They are not a request to move the meeting: they *pin* the one
/// being accepted, and a proposal that has since been superseded is refused
/// (409) rather than committed. `propose` takes no lock and the two calls
/// are minutes apart, so nothing but this pin can tell "the requester
/// agreed to 10:00" from "the requester's click landed on 23:00".
pub async fn accept_proposal(
    db: &Database,
    id: &AppointmentId,
    decided_by: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
) -> Result<Appointment, AppError> {
    // The same approval path re-reads under the lock and reads the proposed
    // window off the row, so accepting is approval — no second code path.
    // Only this door passes `accepting`, and the web layer opens it to the
    // requester alone.
    approve_inner(db, id, decided_by, Some((starts_at, ends_at))).await
}

/// Every retry lost the row to another decision. A 409 rather than a 500: the
/// caller's request was refused, nothing was half-applied, and re-reading the
/// booking shows what happened to it.
fn contended() -> AppError {
    AppError::Conflict("the appointment is being decided elsewhere")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, init_mem};
    use crate::service::appointment_slot;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    /// A window that has not opened yet — `book` refuses started ones.
    fn soon(offset: i64) -> Timestamp {
        at(Timestamp::now().as_millis() + offset)
    }

    #[tokio::test]
    async fn window_prefers_a_proposal_over_the_slot() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let (starts_at, ends_at) = (soon(60_000), soon(120_000));
        let slot = appointment_slot::create(&db, &teacher, starts_at, ends_at, None)
            .await
            .unwrap();
        let mut appointment_row = book(
            &db,
            slot.get_id(),
            &UserId::from_key("s1"),
            AppointmentReason::try_new("ödev").unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(appointment_row.window(&slot), (starts_at, ends_at));
        appointment_row.proposed_starts_at = Some(at(5_000));
        appointment_row.proposed_ends_at = Some(at(6_000));
        assert_eq!(appointment_row.window(&slot), (at(5_000), at(6_000)));
    }

    /// A slot whose window already opened is unbookable: `cancel` would refuse
    /// to undo the resulting meeting, so the booking would be stuck for good.
    #[tokio::test]
    async fn a_started_slot_cannot_be_booked() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let reason = || AppointmentReason::try_new("görüşme").unwrap();
        let started = appointment_slot::create(&db, &teacher, soon(-30_000), soon(60_000), None)
            .await
            .unwrap();
        assert!(matches!(
            book(&db, started.get_id(), &UserId::from_key("s1"), reason()).await,
            Err(AppError::Conflict("the slot has already started"))
        ));

        let upcoming = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        assert!(
            book(&db, upcoming.get_id(), &UserId::from_key("s1"), reason())
                .await
                .is_ok()
        );
    }

    /// Approval judges the *effective* window, so a proposal whose time has
    /// come and gone cannot be accepted — the meeting would be uncancellable.
    #[tokio::test]
    async fn a_started_proposal_cannot_be_accepted() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let student = UserId::from_key("s1");
        let slot = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        let booking = book(
            &db,
            slot.get_id(),
            &student,
            AppointmentReason::try_new("görüşme").unwrap(),
        )
        .await
        .unwrap();

        // Proposing a window that already opened is refused up front — the
        // handler's 60s skew grace would otherwise let one through.
        assert!(matches!(
            propose(&db, booking.get_id(), soon(-30_000), soon(30_000), &teacher).await,
            Err(AppError::Conflict("that time has already started"))
        ));
        // And approval judges the effective window again, for a proposal whose
        // time simply passed while it stood: forced onto the row directly,
        // since `propose` no longer mints one.
        let current = appointment::read(&db, booking.get_id())
            .await
            .unwrap()
            .unwrap();
        let (passed_starts_at, passed_ends_at) = (soon(-30_000), soon(30_000));
        let mut standing = current.clone();
        standing.proposed_starts_at = Some(passed_starts_at);
        standing.proposed_ends_at = Some(passed_ends_at);
        appointment::save_if_unchanged(&db, &current, standing)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            accept_proposal(
                &db,
                booking.get_id(),
                &student,
                passed_starts_at,
                passed_ends_at
            )
            .await,
            Err(AppError::Conflict("that time has already started"))
        ));
        // Refused, not half-applied: still pending, still decidable at a time
        // that can actually happen.
        let after = appointment::read(&db, booking.get_id()).await.unwrap();
        assert_eq!(after.unwrap().get_status(), AppointmentStatus::Pending);
        let (starts_at, ends_at) = (soon(60_000), soon(120_000));
        propose(&db, booking.get_id(), starts_at, ends_at, &teacher)
            .await
            .unwrap();
        // Accepting a window that is *not* the standing proposal is refused —
        // the pin is what the requester saw, not a request to move the meeting.
        assert!(matches!(
            accept_proposal(
                &db,
                booking.get_id(),
                &student,
                passed_starts_at,
                passed_ends_at
            )
            .await,
            Err(AppError::Conflict(
                "the proposed time has changed; \
                 re-read the booking and accept the new one"
            ))
        ));
        let accepted = accept_proposal(&db, booking.get_id(), &student, starts_at, ends_at)
            .await
            .unwrap();
        assert_eq!(accepted.get_status(), AppointmentStatus::Approved);
    }

    /// The compare-and-set every decision writes through — the guard against a
    /// decision built on a snapshot that has since moved, which no lock taken
    /// after the read can catch. The interleaving that matters is the one a
    /// status-only guard would miss, since `propose` leaves a pending booking
    /// pending.
    ///
    /// Deliberately sequential: the mem engine answers `Ok` to a write it then
    /// drops, so a `join!` of two decisions proves nothing. This drives the
    /// primitive itself with the exact stale snapshot the race produces.
    #[tokio::test]
    async fn a_decision_built_on_a_stale_snapshot_never_lands() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let student = UserId::from_key("s1");
        let slot = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        let booking = book(
            &db,
            slot.get_id(),
            &student,
            AppointmentReason::try_new("görüşme").unwrap(),
        )
        .await
        .unwrap();

        // One transaction counter-proposes. A second concurrent transaction
        // is still holding the snapshot it read before that — same id, same
        // `pending` status.
        let stale = booking.clone();
        let (proposed_starts_at, proposed_ends_at) = (soon(180_000), soon(240_000));
        propose(
            &db,
            booking.get_id(),
            proposed_starts_at,
            proposed_ends_at,
            &teacher,
        )
        .await
        .unwrap();

        let mut late = stale.clone();
        late.status = AppointmentStatus::Approved;
        late.decided_by = Some(teacher.clone());
        assert!(
            appointment::save_if_unchanged(&db, &stale, late)
                .await
                .unwrap()
                .is_none()
        );

        // The proposal survived: the row was not confirmed at the slot's old
        // window behind the requester's back.
        let after = appointment::read(&db, booking.get_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.get_status(), AppointmentStatus::Pending);
        assert_eq!(after.get_proposed_starts_at(), Some(proposed_starts_at));
        assert_eq!(after.get_proposed_ends_at(), Some(proposed_ends_at));
        assert_eq!(after.get_decided_by(), None);

        // And the same decision, re-validated against the row as it now
        // stands, does land — a miss is a retry signal, not a wall.
        let mut fresh = after.clone();
        fresh.status = AppointmentStatus::Approved;
        fresh.decided_by = Some(teacher.clone());
        let saved = appointment::save_if_unchanged(&db, &after, fresh)
            .await
            .unwrap()
            .expect("a current snapshot must write");
        assert_eq!(saved.get_status(), AppointmentStatus::Approved);
        assert_eq!(saved.get_proposed_starts_at(), Some(proposed_starts_at));
    }

    /// What the slot row itself says about being taken — the authority every
    /// gate now reads.
    async fn occupied(slot: &AppointmentSlotId, db: &Database) -> i64 {
        let mut result = db
            .query("SELECT VALUE (occupied ?? 0) FROM $slot")
            .bind(("slot", slot.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<i64>>(0).unwrap()[0]
    }

    /// The double-book race with the lock taken out of the picture: a second
    /// booking arrives at the database with nothing between it and the slot but
    /// the claim's own `WHERE`, which is what must refuse it. Driven
    /// through [`cap::claim`] directly rather than through a `join!`, since the
    /// in-memory engine can drop one of two concurrent writes to a record and
    /// still answer `Ok` — this asks the primitive the exact question the race
    /// asks it.
    ///
    /// Deleting the `WHERE (occupied ?? 0) < $cap` from `cap::claim` makes the
    /// second claim succeed and this fail (mutation-proved).
    #[tokio::test]
    async fn a_concurrent_claim_cannot_take_a_booked_slot() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slot = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 0);

        book(
            &db,
            slot.get_id(),
            &UserId::from_key("s1"),
            AppointmentReason::try_new("görüşme").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 1);

        assert!(
            !cap::claim(&slot.get_id().record(), SLOT_OCCUPIED_FIELD, 1, &db)
                .await
                .unwrap(),
            "the slot's seat must already be taken"
        );
        // And the racer's refusal left the stored state alone: one booking, one
        // seat — not two of either.
        assert_eq!(occupied(slot.get_id(), &db).await, 1);
        assert_eq!(
            appointment::list_for_slot(&db, slot.get_id())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The seat and the booking row now land in one transaction, so the stored
    /// state must move together: a taken slot refuses the next requester with
    /// the counter *and* the row list untouched — a refusal that had already
    /// bumped the counter would strand the slot forever.
    #[tokio::test]
    async fn a_refused_booking_moves_nothing() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slot = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        let reason = || AppointmentReason::try_new("görüşme").unwrap();

        book(&db, slot.get_id(), &UserId::from_key("s1"), reason())
            .await
            .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 1);

        let refused = book(&db, slot.get_id(), &UserId::from_key("s2"), reason()).await;
        assert!(
            matches!(refused, Err(AppError::Conflict(_))),
            "the slot is taken"
        );
        assert_eq!(
            occupied(slot.get_id(), &db).await,
            1,
            "the refusal took no seat"
        );
        assert_eq!(
            appointment::list_for_slot(&db, slot.get_id())
                .await
                .unwrap()
                .len(),
            1,
            "and wrote no row"
        );
    }

    /// Cancelling gives the seat back in the same transaction as the status
    /// flip — the counter is the only thing that says a slot is free, so a
    /// cancel that moved one without the other would strand the slot forever.
    #[tokio::test]
    async fn cancelling_hands_the_seat_back_with_the_status() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let student = UserId::from_key("s1");
        let slot = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        let booking = book(
            &db,
            slot.get_id(),
            &student,
            AppointmentReason::try_new("görüşme").unwrap(),
        )
        .await
        .unwrap();
        approve(&db, booking.get_id(), &teacher).await.unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 1, "approval holds it");

        cancel(&db, booking.get_id(), &student, None).await.unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 0);
        // Cancelling twice is refused, so the seat cannot go back twice either.
        assert!(cancel(&db, booking.get_id(), &student, None).await.is_err());
        assert_eq!(occupied(slot.get_id(), &db).await, 0);
        assert!(
            book(
                &db,
                slot.get_id(),
                &UserId::from_key("s2"),
                AppointmentReason::try_new("görüşme").unwrap()
            )
            .await
            .is_ok()
        );
        assert_eq!(occupied(slot.get_id(), &db).await, 1);
    }

    /// Occupancy frees on reject, so the slot can be re-booked.
    #[tokio::test]
    async fn rejecting_frees_the_slot_for_re_booking() {
        let db = init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slot = appointment_slot::create(&db, &teacher, soon(60_000), soon(120_000), None)
            .await
            .unwrap();
        let reason = || AppointmentReason::try_new("görüşme").unwrap();

        let first = book(&db, slot.get_id(), &UserId::from_key("s1"), reason())
            .await
            .unwrap();
        assert!(matches!(
            book(&db, slot.get_id(), &UserId::from_key("s2"), reason()).await,
            Err(AppError::Conflict(_))
        ));
        // The live booking also blocks the slot delete.
        assert!(matches!(
            appointment_slot::delete(&db, slot.clone()).await,
            Err(AppError::Conflict(_))
        ));

        reject(&db, first.get_id(), &teacher, None).await.unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 0);
        let second = book(&db, slot.get_id(), &UserId::from_key("s2"), reason())
            .await
            .unwrap();
        assert_eq!(second.get_status(), AppointmentStatus::Pending);
        assert_eq!(occupied(slot.get_id(), &db).await, 1);
    }
}
