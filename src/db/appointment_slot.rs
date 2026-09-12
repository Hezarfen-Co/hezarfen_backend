//! The `appointment_slot` table: row reads and listings, the role-claimed
//! INSERT every publish writes through, and the delete whose own `WHERE`
//! is the no-live-booking guard. The workflows live in
//! [`crate::service::appointment_slot`].
//!
//! Publish overlap is a database guarantee now: the
//! `appointment_slot_teacher_span` exclusion constraint refuses a second
//! window overlapping one the teacher already holds (SQLSTATE `23505`,
//! mapped to the same 409 the old check produced). The pre-insert reads in
//! [`crate::service::appointment_slot`] remain for their precise refusal
//! texts; the constraint is the authority a racing publish answers to.

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId, SlotNote, SlotSeries};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::{query, query_as};

/// Write a whole publish — one occurrence or fifty-two — while the publisher
/// still *holds* a teaching role.
///
/// Every row here names the teacher, so the write touches no key a demotion
/// touches: [`crate::domain::user::User::set_role`] sweeps the slots its own
/// snapshot can see, and a slot landing after that snapshot would survive it
/// under a role that may not publish — reachable by no route afterwards,
/// since the calendar hides a demoted teacher's slots and the deletes are
/// `teacher`-gated. So the teacher's own row is claimed beside the inserts
/// (the role-handshake recipe in [`crate::db::cap`]): the transaction takes
/// the teacher's row `FOR NO KEY UPDATE` — the same key the demotion
/// writes — and the Rust check refuses unless the role still reaches the
/// teaching bar. Either the sweep sees these slots, or this write sees the
/// new role and publishes nothing.
///
/// Overlap is the `appointment_slot_teacher_span` exclusion constraint: a
/// racing publish or a window colliding with an already-published one
/// refuses the whole batch (`23505`), which the caller maps to its own
/// overlap 409 (`overlap_refusal` carries the exact text the caller owes —
/// single publish and weekly series refuse with different words).
///
/// All-or-nothing: one transaction, so a collision at week 7 of 10 leaves
/// no stray weeks behind.
pub async fn insert_claimed(
    db: &Database,
    teacher: &UserId,
    rows: Vec<AppointmentSlot>,
    overlap_refusal: AppError,
) -> Result<Vec<AppointmentSlot>, AppError> {
    tx_with_retry(db, false, async |tx| {
        // The role handshake. The bar is read off the hierarchy rather than
        // spelled out, so a new role cannot drift out of it.
        let held = sqlx::query!(
            "SELECT role FROM app_user WHERE id = $1 FOR NO KEY UPDATE",
            teacher.uuid()
        )
            .fetch_optional(&mut *tx)
            .await?;
        let fit = held
            .as_ref()
            .and_then(|row| Role::try_from_str(&row.role).ok())
            .is_some_and(|role| role.at_least(Role::Teacher));
        if !fit {
            // Demoted (or gone) while this ran — the same refusal
            // `RequireTeacher` makes a moment earlier, and the only one
            // that can arrive after it.
            return Err(AppError::Forbidden(
                "that account no longer holds a teaching role",
            ));
        }
        let mut saved = Vec::with_capacity(rows.len());
        for row in rows {
            match query_as!(
                AppointmentSlot,
                "INSERT INTO appointment_slot (id, teacher, starts_at, ends_at, note, series, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 RETURNING id AS \"id: AppointmentSlotId\", teacher AS \"teacher: UserId\", starts_at AS \"starts_at: Timestamp\", \
         ends_at AS \"ends_at: Timestamp\", note AS \"note: SlotNote\", \
         series AS \"series: SlotSeries\", created_at AS \"created_at: Timestamp\"",
                row.get_id().uuid(),
                row.get_teacher().uuid(),
                row.get_starts_at().as_millis(),
                row.get_ends_at().as_millis(),
                row.get_note().map(|n| n.as_str().to_string()),
                row.get_series().map(|s| s.as_str().to_string()),
                row.get_created_at().as_millis(),
            )
            .fetch_one(&mut *tx)
            .await
            {
                Ok(row) => saved.push(row),
                // A teacher cannot publish two overlapping windows: the
                // exclusion constraint, answering the caller's overlap 409.
                Err(err) if unique_violation(&err) == Some("appointment_slot_teacher_span") => {
                    return Err(overlap_refusal);
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(saved)
    })
    .await
}

/// Every window this teacher already published that intersects the envelope
/// `[from, to)`. One read for a whole batch: a stored row that overlaps *any*
/// window in the batch necessarily overlaps the envelope spanning them all,
/// so filtering the envelope and comparing in memory is exactly the
/// per-window overlap check hoisted out of the loop. The exclusion
/// constraint remains the authority; this read exists for the batch's
/// precise refusal texts.
pub async fn windows_in_span(
    db: &Database,
    teacher: &UserId,
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<(Timestamp, Timestamp)>, AppError> {
    let rows = query!(
        "SELECT starts_at, ends_at FROM appointment_slot \
         WHERE teacher = $1 AND starts_at < $3 AND ends_at > $2",
        teacher.uuid(),
        from.as_millis(),
        to.as_millis()
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (Timestamp::from_millis(row.starts_at), Timestamp::from_millis(row.ends_at)))
        .collect())
}

pub(crate) async fn read_on<'e, E>(
    executor: E,
    id: &AppointmentSlotId,
) -> Result<Option<AppointmentSlot>, AppError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let slot = query_as!(
        AppointmentSlot,
        "SELECT id AS \"id: AppointmentSlotId\", teacher AS \"teacher: UserId\", starts_at AS \"starts_at: Timestamp\", \
         ends_at AS \"ends_at: Timestamp\", note AS \"note: SlotNote\", \
         series AS \"series: SlotSeries\", created_at AS \"created_at: Timestamp\" \
         FROM appointment_slot WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(executor)
    .await?;
    Ok(slot)
}

pub async fn read(
    db: &Database,
    id: &AppointmentSlotId,
) -> Result<Option<AppointmentSlot>, AppError> {
    let slot = query_as!(
        AppointmentSlot,
        "SELECT id AS \"id: AppointmentSlotId\", teacher AS \"teacher: UserId\", starts_at AS \"starts_at: Timestamp\", \
         ends_at AS \"ends_at: Timestamp\", note AS \"note: SlotNote\", \
         series AS \"series: SlotSeries\", created_at AS \"created_at: Timestamp\" \
         FROM appointment_slot WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(slot)
}

/// A teacher's own calendar, earliest first.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let slots = query_as!(
        AppointmentSlot,
        "SELECT id AS \"id: AppointmentSlotId\", teacher AS \"teacher: UserId\", starts_at AS \"starts_at: Timestamp\", \
         ends_at AS \"ends_at: Timestamp\", note AS \"note: SlotNote\", \
         series AS \"series: SlotSeries\", created_at AS \"created_at: Timestamp\" \
         FROM appointment_slot WHERE teacher = $1 \
         ORDER BY starts_at ASC, id ASC",
        teacher.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(slots)
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
    let slots = query_as!(
        AppointmentSlot,
        "SELECT id AS \"id: AppointmentSlotId\", teacher AS \"teacher: UserId\", starts_at AS \"starts_at: Timestamp\", \
         ends_at AS \"ends_at: Timestamp\", note AS \"note: SlotNote\", \
         series AS \"series: SlotSeries\", created_at AS \"created_at: Timestamp\" \
         FROM appointment_slot WHERE starts_at > $1 \
         ORDER BY starts_at ASC, id ASC",
        from.as_millis()
    )
    .fetch_all(db)
    .await?;
    Ok(slots)
}

pub async fn list_for_series(
    db: &Database,
    series: &SlotSeries,
) -> Result<Vec<AppointmentSlot>, AppError> {
    let slots = query_as!(
        AppointmentSlot,
        "SELECT id AS \"id: AppointmentSlotId\", teacher AS \"teacher: UserId\", starts_at AS \"starts_at: Timestamp\", \
         ends_at AS \"ends_at: Timestamp\", note AS \"note: SlotNote\", \
         series AS \"series: SlotSeries\", created_at AS \"created_at: Timestamp\" \
         FROM appointment_slot WHERE series = $1 \
         ORDER BY starts_at ASC, id ASC",
        series.as_str()
    )
    .fetch_all(db)
    .await?;
    Ok(slots)
}

/// Shared body of both deletes: drop every named slot, but only while its
/// `occupied` counter says nobody is waiting on it — the delete's own
/// `WHERE` is the guard, which is what makes it hold against a booking
/// landing in the middle of it (a separate read-then-delete could not).
///
/// Settled bookings (rejected/cancelled) of the free slots are swept first
/// so the parent delete's foreign key is never the thing that refuses. One
/// transaction, all-or-nothing across the whole list: if fewer rows go than
/// were named, the transaction is thrown away, so a series never loses its
/// free weeks and keeps the booked one. Whether that shortfall was a live
/// booking or a slot that no longer exists is read off what survived — the
/// occupied rows are still there, a vanished one is not — which is the
/// 409/404 the caller expects.
pub async fn delete_free(db: &Database, ids: &[AppointmentSlotId]) -> Result<(), AppError> {
    tx_with_retry(db, false, async |tx| {
        // The settled bookings of the *free* slots go first; a booked slot's
        // rows survive (its occupied counter excludes it from both sweeps).
        sqlx::query!(
            "DELETE FROM appointment a
             USING appointment_slot s
             WHERE a.slot = s.id AND s.id = ANY($1) AND s.occupied < 1",
            ids
        )
        .execute(&mut *tx)
        .await?;
        let gone: Vec<AppointmentSlotId> = query_as!(
            AppointmentSlotId,
            "DELETE FROM appointment_slot WHERE id = ANY($1) AND occupied < 1 RETURNING id",
            ids
        )
        .fetch_all(&mut *tx)
        .await?;
        if gone.len() == ids.len() {
            return Ok(());
        }
        // Shortfall: distinguish "a live booking holds one" from "one was
        // never there" off what still exists.
        let remaining = sqlx::query!(
            "SELECT count(*) AS standing FROM appointment_slot WHERE id = ANY($1)",
            ids
        )
        .fetch_one(&mut *tx)
        .await?
        .standing;
        if remaining > 0 {
            Err(AppError::Conflict(
                "the slot has a pending or approved booking",
            ))
        } else {
            Err(AppError::NotFound)
        }
    })
    .await
}

// The overlap probe behind the old publish guard — `conflicts_existing` —
// is gone as a *decision*: the exclusion constraint decides overlap at
// insert time. The service layer still reads [`windows_in_span`] ahead of a
// weekly publish so its precise refusal texts survive; a racing writer that
// slips past any read answers to the constraint.
