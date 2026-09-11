//! The `appointment_slot` table: row reads and listings, the overlap probes
//! behind the publish guard, the role-claimed INSERT every publish writes
//! through, and the delete whose own `WHERE` is the no-live-booking guard.
//! The workflows that sequence these under
//! [`crate::service::appointment::APPOINTMENT_LOCK`] live in
//! [`crate::service::appointment_slot`].

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::{APPOINTMENT_SLOT_TABLE, ROLES};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId, SlotSeries};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The publisher fell below `teacher` while the publish was in flight.
const UNFIT_MARK: &str = "slot_not_staff";

/// Write a whole publish — one occurrence or fifty-two — while the publisher
/// still *holds* a teaching role.
///
/// Every row here names the teacher, so the write touches no key a demotion
/// touches: [`crate::domain::user::User::set_role`] sweeps the slots its own
/// snapshot can see, and a slot landing after that snapshot would survive it
/// under a role that may not publish — reachable by no route afterwards,
/// since the calendar hides a demoted teacher's slots and the deletes are
/// `teacher`-gated. So the teacher's own record is claimed beside the insert
/// ([`cap::role_claim`]), which is the key the demotion writes: either the
/// sweep sees these slots, or this write sees the new role and publishes
/// nothing.
///
/// The bar is read off the hierarchy rather than spelled out, so a new role
/// cannot drift out of it.
///
/// Admissible for [`transaction_with_retry`]: the claim is `SELECT`/`UPDATE`
/// only, and the `INSERT` carries freshly minted ULIDs on a table with no
/// `UNIQUE` index, so no rival can make it answer "already exists".
pub async fn insert_claimed(
    db: &Database,
    teacher: &UserId,
    rows: Vec<AppointmentSlot>,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let staff = ROLES
        .iter()
        .filter(|role| role.at_least(Role::Teacher))
        .map(|role| format!("'{}'", role.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let held = cap::role_claim("teacher", &format!("NOT IN [{staff}]"), UNFIT_MARK);
    let sql = format!(
        "BEGIN TRANSACTION;\n{};\nINSERT INTO {APPOINTMENT_SLOT_TABLE} $rows;\n\
         COMMIT TRANSACTION;",
        held.join(";\n")
    );
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[
            ("teacher".into(), teacher.record().into_value()),
            ("rows".into(), rows.into_value()),
        ],
        &[UNFIT_MARK],
    )
    .await?;
    // Demoted while this ran — the same refusal `RequireTeacher` makes a
    // moment earlier, and the only one that can arrive after it.
    if errors
        .values()
        .any(|error| error.to_string().contains(UNFIT_MARK))
    {
        return Err(AppError::Forbidden(
            "that account no longer holds a teaching role",
        ));
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // BEGIN plus the claim's own statements — counted, not tallied by hand,
    // so a statement added there cannot read back the wrong result.
    Ok(result.take::<Vec<AppointmentSlot>>(held.len() + 1)?)
}

/// Does the teacher already have a published slot whose window collides with
/// `[starts_at, ends_at)`? Half-open, so a slot ending exactly where the new
/// one starts is *not* a conflict — that is how a teacher's hour is carved
/// into back-to-back slots. Caller must hold
/// [`crate::service::appointment::APPOINTMENT_LOCK`] for the
/// answer to still be true by the time the insert lands.
pub async fn conflicts_existing(
    db: &Database,
    teacher: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
) -> Result<bool, AppError> {
    let mut result = db
        .query(
            "SELECT VALUE id FROM appointment_slot \
             WHERE teacher = $teacher AND starts_at < $ends AND ends_at > $starts \
             LIMIT 1",
        )
        .bind(("teacher", teacher.record()))
        .bind(("starts", starts_at.as_millis()))
        .bind(("ends", ends_at.as_millis()))
        .await?
        .check()?;
    Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
}

/// Every window this teacher already published that intersects the envelope
/// `[from, to)`. One read for a whole batch: a stored row that overlaps *any*
/// window in the batch necessarily overlaps the envelope spanning them all,
/// so filtering the envelope and comparing in memory is exactly the
/// per-window [`conflicts_existing`] check,
/// hoisted out of the loop — which is what keeps
/// [`crate::service::appointment::APPOINTMENT_LOCK`] down
/// to two round trips instead of one per occurrence.
pub async fn windows_in_span(
    db: &Database,
    teacher: &UserId,
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<(Timestamp, Timestamp)>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM appointment_slot \
             WHERE teacher = $teacher AND starts_at < $to AND ends_at > $from",
        )
        .bind(("teacher", teacher.record()))
        .bind(("from", from.as_millis()))
        .bind(("to", to.as_millis()))
        .await?
        .check()?;
    Ok(result
        .take::<Vec<AppointmentSlot>>(0)?
        .into_iter()
        .map(|slot| (slot.starts_at, slot.ends_at))
        .collect())
}

pub async fn read(
    db: &Database,
    id: &AppointmentSlotId,
) -> Result<Option<AppointmentSlot>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// A teacher's own calendar, earliest first.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM appointment_slot WHERE teacher = $teacher \
             ORDER BY starts_at ASC, id ASC",
        )
        .bind(("teacher", teacher.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<AppointmentSlot>>(0)?)
}

/// Every slot whose window has not opened yet, earliest first — the bookable
/// calendar a requester browses.
///
/// The bound is `starts_at`, not `ends_at`, so that this window is exactly
/// the one [`crate::service::appointment::book`] accepts: a slot already underway is
/// unbookable (`cancel` could never undo the booking), and offering it would
/// be a calendar entry that can only answer `409`. The publish-time 60s skew
/// grace deliberately does not apply here either — `book` does not grant it,
/// and a slot published a moment after its own start is unbookable from
/// birth, so it belongs on nobody's calendar.
pub async fn list_upcoming(
    db: &Database,
    from: Timestamp,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM appointment_slot WHERE starts_at > $from \
             ORDER BY starts_at ASC, id ASC",
        )
        .bind(("from", from.as_millis()))
        .await?
        .check()?;
    Ok(result.take::<Vec<AppointmentSlot>>(0)?)
}

pub async fn list_for_series(
    db: &Database,
    series: &SlotSeries,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM appointment_slot WHERE series = $series \
             ORDER BY starts_at ASC, id ASC",
        )
        .bind(("series", series.as_str().to_string()))
        .await?
        .check()?;
    Ok(result.take::<Vec<AppointmentSlot>>(0)?)
}

/// Shared body of both deletes: drop every named slot, but only while its
/// `occupied` counter says nobody is waiting on it — the delete's own
/// `WHERE` is the guard, which is what makes it hold against a booking
/// landing in the middle of it (a separate read-then-delete could not).
///
/// All-or-nothing across the whole list: if fewer rows go than were named,
/// the transaction is thrown away, so a series never loses its free weeks
/// and keeps the booked one. Whether that shortfall was a live booking or a
/// slot that no longer exists is read off what survived — the occupied rows
/// are still there, a vanished one is not — which is the 409/404 the caller
/// used to get from a separate check.
pub async fn delete_free(db: &Database, ids: &[AppointmentSlotId]) -> Result<(), AppError> {
    let records: Vec<RecordId> = ids.iter().map(|id| id.record()).collect();
    let (_, mut errors) = transaction_with_retry(
        db,
        "BEGIN TRANSACTION;
             LET $gone = (DELETE $slots WHERE (occupied ?? 0) = 0 RETURN BEFORE);
             IF array::len($gone) != array::len($slots) {
                 THROW IF array::len((SELECT VALUE id FROM appointment_slot
                     WHERE id IN $slots)) > 0 { 'slot_occupied' } ELSE { 'slot_missing' }
             };
             DELETE appointment WHERE slot IN $slots;
             RETURN $gone;
             COMMIT TRANSACTION;",
        &[("slots".into(), records.into_value())],
        &["slot_occupied", "slot_missing"],
    )
    .await?;
    // An aborted transaction errors every slot; only the THROW's own slot
    // names the marker (the `Exam::update` treatment), and a round lost to
    // a booking landing on `occupied` mid-flight is re-sent rather than
    // reported (see [`transaction_with_retry`]).
    let thrown = |marker: &str| {
        errors
            .values()
            .any(|error| error.to_string().contains(marker))
    };
    if thrown("slot_occupied") {
        return Err(AppError::Conflict(
            "the slot has a pending or approved booking",
        ));
    }
    if thrown("slot_missing") {
        return Err(AppError::NotFound);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    Ok(())
}
