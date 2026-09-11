//! The `appointment` table: row reads and listings, the overlap probe behind
//! the booking guard, and the compare-and-set every decision writes through.
//! The workflows that sequence these under
//! [`crate::service::appointment::APPOINTMENT_LOCK`] live in
//! [`crate::service::appointment`].

use surrealdb::types::SurrealValue;

use crate::database::{Database, lost_the_race};
use crate::db::page::PagedList;
use crate::domain::appointment::{Appointment, AppointmentId, AppointmentStatus};
use crate::domain::appointment_slot::AppointmentSlotId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The window an approved booking actually occupies, as read back from the
/// database: the counter-proposed time when there is one, the slot's own
/// otherwise. Optional fields because a dangling slot reference resolves to
/// `NONE` — such a row is skipped rather than failing the whole guard.
#[derive(Debug, Clone, SurrealValue)]
struct BookedWindow {
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
}

/// Is `[starts_at, ends_at)` already taken for either party? Both sides are
/// checked: a teacher can't be in two meetings at once, and neither can a
/// requester (a parent with two children's teachers, say).
///
/// Only *approved* bookings block — a pending request is a wish, not a
/// commitment, so several may queue on overlapping times and the first
/// approval wins. `except` skips one row, so re-checking a booking against
/// the world never trips over itself.
///
/// Caller must hold
/// [`crate::service::appointment::APPOINTMENT_LOCK`] for the answer to still
/// be true by the time it is acted on.
pub async fn conflicts(
    db: &Database,
    teacher: &UserId,
    requester: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    except: Option<&AppointmentId>,
) -> Result<bool, AppError> {
    // The effective window is computed in the query (proposal overrides
    // the slot) but compared here: one round trip, and the comparison
    // itself stays a plain, unit-tested Rust predicate.
    let mut result = db
        .query(
            "SELECT \
               (IF proposed_starts_at != NONE THEN proposed_starts_at \
                ELSE slot.starts_at END) AS starts_at, \
               (IF proposed_ends_at != NONE THEN proposed_ends_at \
                ELSE slot.ends_at END) AS ends_at \
             FROM appointment \
             WHERE status = 'approved' AND id != $except \
               AND (requester = $requester OR slot.teacher = $teacher)",
        )
        .bind(("teacher", teacher.record()))
        .bind(("requester", requester.record()))
        .bind(("except", except.map(AppointmentId::record)))
        .await?
        .check()?;
    Ok(result
        .take::<Vec<BookedWindow>>(0)?
        .into_iter()
        .filter_map(|window| Some((window.starts_at?, window.ends_at?)))
        .any(|(booked_starts_at, booked_ends_at)| {
            Appointment::overlaps(starts_at, ends_at, booked_starts_at, booked_ends_at)
        }))
}

pub async fn read(
    db: &Database,
    id: &AppointmentId,
) -> Result<Option<Appointment>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// The row, insisting it is still open for a decision.
pub async fn read_pending(db: &Database, id: &AppointmentId) -> Result<Appointment, AppError> {
    let appointment = read(db, id).await?.ok_or(AppError::NotFound)?;
    if appointment.status != AppointmentStatus::Pending {
        return Err(AppError::Conflict("the appointment is no longer pending"));
    }
    Ok(appointment)
}

pub async fn list_for_requester(
    db: &Database,
    requester: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Appointment>, i64), AppError> {
    PagedList::new(
        "appointment WHERE requester = $requester",
        "ORDER BY id DESC",
    )
    .bind("requester", requester.record())
    .run(limit, offset, db)
    .await
}

/// Every booking aimed at `teacher`, across all their slots — the teacher's
/// request inbox.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Appointment>, i64), AppError> {
    PagedList::new(
        "appointment WHERE slot.teacher = $teacher",
        "ORDER BY id DESC",
    )
    .bind("teacher", teacher.record())
    .run(limit, offset, db)
    .await
}

pub async fn list_for_slot(
    db: &Database,
    slot: &AppointmentSlotId,
) -> Result<Vec<Appointment>, AppError> {
    let mut result = db
        .query("SELECT * FROM appointment WHERE slot = $slot ORDER BY id DESC")
        .bind(("slot", slot.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Appointment>>(0)?)
}

/// Write the decision only while the stored row still carries the state it
/// was validated against. `None` means it does not: another decision landed
/// between the read and the write, so reload, re-validate, and try again.
///
/// Four columns discriminate every write, and `status` alone would not:
/// `propose` leaves a pending booking pending, so an approval built from a
/// snapshot taken *before* that proposal would sail through a status-only
/// guard and confirm the meeting at the old, superseded time. With the
/// proposed window and `decided_by` alongside it, every mutator moves at
/// least one of the four. The fields only a terminal transition writes
/// (`cancelled_by`, `cancel_reason`, `reject_reason`) need no compare of
/// their own: they always travel with a `status` change into a state no
/// later decision is allowed from. All four are top-level columns, so an
/// absent one reads back as `NONE` and `NONE = NONE` holds — the object-key
/// dropping that forces `settings` to compare a rebuilt projection cannot
/// bite here.
///
/// A store-level write conflict is reported the same way as a miss: it
/// means a concurrent write committed on this row, which is exactly the
/// "reload and re-decide" case.
///
/// A transition out of a live status (reject, cancel) hands the slot's
/// `occupied` seat back, in this same transaction — the counter is the
/// authority on whether the slot is taken, so it must move if and only if
/// the status did. The decrement is `WHERE`-gated on the CAS having written
/// something, so a lost race frees nothing.
pub async fn save_if_unchanged(
    db: &Database,
    expected: &Appointment,
    new: Appointment,
) -> Result<Option<Appointment>, AppError> {
    let frees_the_slot = expected.status.is_live() && !new.status.is_live();
    let attempted = async {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $done = (UPDATE $id CONTENT $new \
                 WHERE status = $status \
                   AND proposed_starts_at = $proposed_starts_at \
                   AND proposed_ends_at = $proposed_ends_at \
                   AND decided_by = $decided_by);
                 UPDATE $slot SET occupied = math::max([(occupied ?? 0) - 1, 0])
                   WHERE $frees AND array::len($done) > 0;
                 RETURN $done;
                 COMMIT TRANSACTION;",
            )
            .bind(("slot", expected.slot.record()))
            .bind(("frees", frees_the_slot))
            .bind(("id", expected.id.record()))
            .bind(("status", expected.status))
            .bind(("proposed_starts_at", expected.proposed_starts_at))
            .bind(("proposed_ends_at", expected.proposed_ends_at))
            .bind(("decided_by", expected.decided_by.clone()))
            .bind(("new", new))
            .await?
            .check()?;
        // BEGIN, the LET, and the counter write take a slot each.
        result.take::<Vec<Appointment>>(3)
    }
    .await;
    match attempted {
        Ok(rows) => Ok(rows.into_iter().next()),
        Err(err) if lost_the_race(&err) => Ok(None),
        Err(err) => Err(err.into()),
    }
}
