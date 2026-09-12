//! The `appointment` table: row reads and listings, the overlap probe behind
//! the booking guard, the seat claim the booking writes through, and the
//! compare-and-set every decision writes. The workflows live in
//! [`crate::service::appointment`] (no lock anymore: occupancy is a
//! conditional counter write, the decision CAS is one statement, and the
//! approval's overlap decision is a serializable transaction documented at
//! its own site).

use crate::database::{Database, tx_with_retry};
use crate::db::cap::Claimed;
use crate::db::page::PagedList;
use crate::domain::appointment::{Appointment, AppointmentId, AppointmentReason, AppointmentStatus};
use crate::domain::appointment_slot::AppointmentSlotId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::{query, query_as, PgConnection, Postgres};

/// The appointment row's columns, in [`Appointment`]'s field order.
const COLS: &str = "id, slot, requester, status, reason, proposed_starts_at, proposed_ends_at, \
                    proposed_by, decided_by, cancelled_by, cancel_reason, reject_reason, created_at";

/// Is `[starts_at, ends_at)` already taken for either party? Both sides are
/// checked: a teacher can't be in two meetings at once, and neither can a
/// requester (a parent with two children's teachers, say).
///
/// Only *approved* bookings block — a pending request is a wish, not a
/// commitment, so several may queue on overlapping times and the first
/// approval wins. `except` skips one row, so re-checking a booking against
/// the world never trips over itself.
///
/// The effective window (the proposal when one stands, the slot's own
/// otherwise) is computed in the query and compared in Rust: one round
/// trip, and the comparison stays a plain, unit-tested predicate. A booking
/// whose slot was withdrawn since (no window left to stand on) is skipped.
pub async fn conflicts(
    db: &Database,
    teacher: &UserId,
    requester: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    except: Option<&AppointmentId>,
) -> Result<bool, AppError> {
    conflicts_on(db, teacher, requester, starts_at, ends_at, except).await
}

/// [`conflicts`] against any executor — the approval decision re-asks this
/// inside its own transaction, so the verdict it acts on is the one its
/// snapshot holds.
pub(crate) async fn conflicts_on<'e, E>(
    executor: E,
    teacher: &UserId,
    requester: &UserId,
    starts_at: Timestamp,
    ends_at: Timestamp,
    except: Option<&AppointmentId>,
) -> Result<bool, AppError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let windows = query!(
        "SELECT COALESCE(a.proposed_starts_at, s.starts_at) AS starts_at, \
                COALESCE(a.proposed_ends_at, s.ends_at) AS ends_at \
         FROM appointment a JOIN appointment_slot s ON a.slot = s.id \
         WHERE a.status = 'approved' \
           AND ($3::uuid IS NULL OR a.id <> $3) \
           AND (a.requester = $2 OR s.teacher = $1)",
        teacher.uuid(),
        requester.uuid(),
        except.map(|id| id.uuid()),
    )
    .fetch_all(executor)
    .await?;
    Ok(windows
        .into_iter()
        .filter_map(|window| {
            Some((
                Timestamp::from_millis(window.starts_at?),
                Timestamp::from_millis(window.ends_at?),
            ))
        })
        .any(|(booked_starts_at, booked_ends_at)| {
            Appointment::overlaps(starts_at, ends_at, booked_starts_at, booked_ends_at)
        }))
}

pub(crate) async fn read_on<'e, E>(executor: E, id: &AppointmentId) -> Result<Option<Appointment>, AppError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let appointment = query_as!(
        Appointment,
        "SELECT id AS \"id: AppointmentId\", slot AS \"slot: AppointmentSlotId\", requester AS \"requester: UserId\", \
                   status AS \"status: AppointmentStatus\", reason AS \"reason: AppointmentReason\", \
                   proposed_starts_at AS \"proposed_starts_at: Timestamp\", \
                   proposed_ends_at AS \"proposed_ends_at: Timestamp\", \
                   proposed_by AS \"proposed_by: UserId\", decided_by AS \"decided_by: UserId\", \
                   cancelled_by AS \"cancelled_by: UserId\", \
                   cancel_reason AS \"cancel_reason: AppointmentReason\", \
                   reject_reason AS \"reject_reason: AppointmentReason\", \
                   created_at AS \"created_at: Timestamp\" \
         FROM appointment WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(executor)
    .await?;
    Ok(appointment)
}

pub async fn read(
    db: &Database,
    id: &AppointmentId,
) -> Result<Option<Appointment>, AppError> {
    read_on(db, id).await
}

/// The row, insisting it is still open for a decision.
pub async fn read_pending(db: &Database, id: &AppointmentId) -> Result<Appointment, AppError> {
    read_pending_on(db, id).await
}

/// [`read_pending`] against any executor (the approval's transaction).
pub(crate) async fn read_pending_on<'e, E>(
    executor: E,
    id: &AppointmentId,
) -> Result<Appointment, AppError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let appointment = read_on(executor, id).await?.ok_or(AppError::NotFound)?;
    if appointment.get_status() != AppointmentStatus::Pending {
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
    PagedList::new("appointment WHERE requester = $1", "ORDER BY id DESC")
        .bind(requester.uuid())
        .run(limit, offset, db)
        .await
}

/// Every booking aimed at `teacher`, across all their slots — the
/// teacher's request inbox. The teacher lives on the slot row now, so
/// this is a join; the page and its count share the one predicate, and
/// the columns are spelled out so the join's duplicate slot-column names
/// can never leak into the row decode.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Appointment>, i64), AppError> {
    match limit {
        None => {
            let rows = query_as!(
                Appointment,
                "SELECT a.id AS \"id: AppointmentId\", a.slot AS \"slot: AppointmentSlotId\", a.requester AS \"requester: UserId\", \
                        a.status AS \"status: AppointmentStatus\", a.reason AS \"reason: AppointmentReason\", \
                        a.proposed_starts_at AS \"proposed_starts_at: Timestamp\", \
                        a.proposed_ends_at AS \"proposed_ends_at: Timestamp\", \
                        a.proposed_by AS \"proposed_by: UserId\", a.decided_by AS \"decided_by: UserId\", \
                        a.cancelled_by AS \"cancelled_by: UserId\", \
                        a.cancel_reason AS \"cancel_reason: AppointmentReason\", \
                        a.reject_reason AS \"reject_reason: AppointmentReason\", \
                        a.created_at AS \"created_at: Timestamp\" \
                 FROM appointment a JOIN appointment_slot s ON a.slot = s.id \
                 WHERE s.teacher = $1 \
                 ORDER BY a.id DESC",
                teacher.uuid()
            )
            .fetch_all(db)
            .await?;
            Ok((rows, rows.len() as i64))
        }
        Some(limit) => {
            let rows = query_as!(
                Appointment,
                "SELECT a.id AS \"id: AppointmentId\", a.slot AS \"slot: AppointmentSlotId\", a.requester AS \"requester: UserId\", \
                        a.status AS \"status: AppointmentStatus\", a.reason AS \"reason: AppointmentReason\", \
                        a.proposed_starts_at AS \"proposed_starts_at: Timestamp\", \
                        a.proposed_ends_at AS \"proposed_ends_at: Timestamp\", \
                        a.proposed_by AS \"proposed_by: UserId\", a.decided_by AS \"decided_by: UserId\", \
                        a.cancelled_by AS \"cancelled_by: UserId\", \
                        a.cancel_reason AS \"cancel_reason: AppointmentReason\", \
                        a.reject_reason AS \"reject_reason: AppointmentReason\", \
                        a.created_at AS \"created_at: Timestamp\" \
                 FROM appointment a JOIN appointment_slot s ON a.slot = s.id \
                 WHERE s.teacher = $1 \
                 ORDER BY a.id DESC LIMIT $2 OFFSET $3",
                teacher.uuid(),
                limit,
                offset
            )
            .fetch_all(db)
            .await?;
            let total = query!(
                "SELECT count(*) AS total \
                 FROM appointment a JOIN appointment_slot s ON a.slot = s.id \
                 WHERE s.teacher = $1",
                teacher.uuid()
            )
            .fetch_one(db)
            .await?
            .total;
            Ok((rows, total))
        }
    }
}

pub async fn list_for_slot(
    db: &Database,
    slot: &AppointmentSlotId,
) -> Result<Vec<Appointment>, AppError> {
    let rows = query_as!(
        Appointment,
        "SELECT id AS \"id: AppointmentId\", slot AS \"slot: AppointmentSlotId\", requester AS \"requester: UserId\", \
                   status AS \"status: AppointmentStatus\", reason AS \"reason: AppointmentReason\", \
                   proposed_starts_at AS \"proposed_starts_at: Timestamp\", \
                   proposed_ends_at AS \"proposed_ends_at: Timestamp\", \
                   proposed_by AS \"proposed_by: UserId\", decided_by AS \"decided_by: UserId\", \
                   cancelled_by AS \"cancelled_by: UserId\", \
                   cancel_reason AS \"cancel_reason: AppointmentReason\", \
                   reject_reason AS \"reject_reason: AppointmentReason\", \
                   created_at AS \"created_at: Timestamp\" \
         FROM appointment WHERE slot = $1 ORDER BY id DESC",
        slot.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Request `slot`: take the slot's one seat and write the pending booking
/// in a single statement — two racing requests cannot both find the slot
/// free however they interleave, and a refused insert takes its own seat
/// bump back with it. Zero rows means "full, or the slot was deleted since
/// the caller read it": the marker is one, and the caller re-reads to pick
/// the 404/409.
pub async fn claim_slot_and_create(
    db: &Database,
    appointment: Appointment,
) -> Result<Claimed<Appointment>, AppError> {
    match query_as!(
        Appointment,
        "WITH seat AS (
             UPDATE appointment_slot SET occupied = occupied + 1
             WHERE id = $1 AND occupied < 1
             RETURNING 1)
         INSERT INTO appointment (id, slot, requester, status, reason, proposed_starts_at, \
                                  proposed_ends_at, proposed_by, decided_by, cancelled_by, \
                                  cancel_reason, reject_reason, created_at)
         SELECT $2, $1, $3, 'pending', $4, NULL, NULL, NULL, NULL, NULL, NULL, NULL, $5
         WHERE EXISTS (SELECT 1 FROM seat)
         RETURNING id AS \"id: AppointmentId\", slot AS \"slot: AppointmentSlotId\", requester AS \"requester: UserId\", \
                   status AS \"status: AppointmentStatus\", reason AS \"reason: AppointmentReason\", \
                   proposed_starts_at AS \"proposed_starts_at: Timestamp\", \
                   proposed_ends_at AS \"proposed_ends_at: Timestamp\", \
                   proposed_by AS \"proposed_by: UserId\", decided_by AS \"decided_by: UserId\", \
                   cancelled_by AS \"cancelled_by: UserId\", \
                   cancel_reason AS \"cancel_reason: AppointmentReason\", \
                   reject_reason AS \"reject_reason: AppointmentReason\", \
                   created_at AS \"created_at: Timestamp\"",
        appointment
            .get_slot()
            .map(|s| s.uuid())
            .expect("a new booking always carries its slot"),
        appointment.get_id().uuid(),
        appointment.get_requester().uuid(),
        appointment.get_reason().as_str(),
        appointment.get_created_at().as_millis(),
    )
    .fetch_optional(db)
    .await
    {
        Ok(Some(row)) => Ok(Claimed::Made(row)),
        Ok(None) => Ok(Claimed::Full),
        // The id is freshly minted on a table keyed by it; no rival can
        // have aimed at it, and the seat gate excludes any slot the insert
        // could dangle from — no other verdict exists on this path.
        Err(err) => Err(err.into()),
    }
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
/// later decision is allowed from. Absent values compare truthfully under
/// `IS NOT DISTINCT FROM`.
///
/// A transition out of a live status (reject, cancel) hands the slot's
/// `occupied` seat back in the same transaction — the counter is the
/// authority on whether the slot is taken, so it must move if and only if
/// the status did. The decrement is gated on the CAS having written
/// something, so a lost race frees nothing.
pub async fn save_if_unchanged(
    db: &Database,
    expected: &Appointment,
    new: Appointment,
) -> Result<Option<Appointment>, AppError> {
    tx_with_retry(db, false, async |tx| {
        decision_cas(&mut *tx, expected, new.clone()).await
    })
    .await
}

/// The decision's compare-and-set against any executor: one conditional
/// `UPDATE`, plus the seat release when the status left the live set. A
/// single statement decides under Postgres's row locking, so no retry
/// budget and no store-level "write conflict" class exists anymore — a miss
/// is simply `None`, the reload-and-re-decide answer.
pub(crate) async fn decision_cas(
    tx: &mut PgConnection,
    expected: &Appointment,
    new: Appointment,
) -> Result<Option<Appointment>, AppError> {
    let frees_the_slot = expected.get_status().is_live() && !new.get_status().is_live();
    let done = query_as!(
        Appointment,
        "UPDATE appointment SET status = $2, proposed_starts_at = $3, proposed_ends_at = $4, \
                proposed_by = $5, decided_by = $6, cancelled_by = $7, cancel_reason = $8, \
                reject_reason = $9 \
         WHERE id = $1 AND status = $10 \
           AND proposed_starts_at IS NOT DISTINCT FROM $11 \
           AND proposed_ends_at IS NOT DISTINCT FROM $12 \
           AND decided_by IS NOT DISTINCT FROM $13 \
         RETURNING id AS \"id: AppointmentId\", slot AS \"slot: AppointmentSlotId\", requester AS \"requester: UserId\", \
                   status AS \"status: AppointmentStatus\", reason AS \"reason: AppointmentReason\", \
                   proposed_starts_at AS \"proposed_starts_at: Timestamp\", \
                   proposed_ends_at AS \"proposed_ends_at: Timestamp\", \
                   proposed_by AS \"proposed_by: UserId\", decided_by AS \"decided_by: UserId\", \
                   cancelled_by AS \"cancelled_by: UserId\", \
                   cancel_reason AS \"cancel_reason: AppointmentReason\", \
                   reject_reason AS \"reject_reason: AppointmentReason\", \
                   created_at AS \"created_at: Timestamp\"",
        new.get_id().uuid(),
        new.get_status().as_str(),
        new.get_proposed_starts_at().map(|t| t.as_millis()),
        new.get_proposed_ends_at().map(|t| t.as_millis()),
        new.get_proposed_by().map(|u| u.uuid()),
        new.get_decided_by().map(|u| u.uuid()),
        new.get_cancelled_by().map(|u| u.uuid()),
        new.get_cancel_reason().map(|r| r.as_str().to_string()),
        new.get_reject_reason().map(|r| r.as_str().to_string()),
        expected.get_status().as_str(),
        expected.get_proposed_starts_at().map(|t| t.as_millis()),
        expected.get_proposed_ends_at().map(|t| t.as_millis()),
        expected.get_decided_by().map(|u| u.uuid()),
    )
    .fetch_optional(&mut *tx)
    .await?;
    if frees_the_slot && done.is_some() {
        sqlx::query!(
            "UPDATE appointment_slot SET occupied = GREATEST(occupied - 1, 0) WHERE id = $1",
            expected.get_slot().map(|s| s.uuid()),
        )
        .execute(tx)
        .await?;
    }
    Ok(done)
}
