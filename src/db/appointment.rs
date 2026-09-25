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
use crate::domain::appointment::{
    Appointment, AppointmentId, AppointmentReason, AppointmentStatus,
};
use crate::domain::appointment_slot::AppointmentSlotId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::{PgConnection, Postgres, query, query_as};

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

pub(crate) async fn read_on<'e, E>(
    executor: E,
    id: &AppointmentId,
) -> Result<Option<Appointment>, AppError>
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

pub async fn read(db: &Database, id: &AppointmentId) -> Result<Option<Appointment>, AppError> {
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

/// The optional filters and the page of [`list_for_requester`], in one
/// struct — the fields outgrew a readable argument list. The default is
/// the unfiltered, unpaged read.
#[derive(Default)]
pub struct RequesterListParams<'a> {
    pub status: Option<AppointmentStatus>,
    /// The meeting's effective start bounds, `[after, before)`.
    pub starts_after: Option<Timestamp>,
    pub starts_before: Option<Timestamp>,
    /// The booked slot's teacher.
    pub teacher: Option<&'a UserId>,
    /// `None` = the full list.
    pub limit: Option<i64>,
    pub offset: i64,
}

/// The caller's own bookings, newest first. The optional filters narrow the
/// read: `status` matches the stored state, `starts_after`/`starts_before`
/// bound the meeting's effective start — a standing proposal, the slot's
/// window otherwise — from below (inclusive) and above (exclusive, so a
/// month is `[start, next_start)`), and `teacher` is the booked slot's
/// teacher — which needs the slot row, so the read runs over a derived table
/// the window and its count share. A booking whose slot is gone has no
/// meeting time and no teacher, so any of those three filters drops it; with
/// no filter the read stays the plain single-table one, so today's rows
/// (dangling bookings included) are untouched.
pub async fn list_for_requester(
    db: &Database,
    requester: &UserId,
    params: RequesterListParams<'_>,
) -> Result<(Vec<Appointment>, i64), AppError> {
    let RequesterListParams {
        status,
        starts_after,
        starts_before,
        teacher,
        limit,
        offset,
    } = params;
    // Spell the appointment columns against the join alias, so the derived
    // table's row keeps the exact names [`Appointment`] decodes and the
    // slot's duplicate column names never leak in. The plain shape aliases
    // the table too, so the one clause builder speaks both shapes.
    const JOINED: &str = "(SELECT a.id, a.slot, a.requester, a.status, a.reason, \
         a.proposed_starts_at, a.proposed_ends_at, a.proposed_by, a.decided_by, \
         a.cancelled_by, a.cancel_reason, a.reject_reason, a.created_at \
         FROM appointment a LEFT JOIN appointment_slot s ON a.slot = s.id \
         WHERE a.requester = $1";
    let joined = teacher.is_some() || starts_after.is_some() || starts_before.is_some();
    let mut from_where = if joined {
        JOINED.to_string()
    } else {
        "appointment a WHERE a.requester = $1".to_string()
    };
    // The meeting's start: a standing proposal overrides the slot's window
    // (the same rule [`Appointment::window`] applies), so the predicate
    // coalesces them — and these clauses only ever ride the joined shape,
    // where the slot row is in hand.
    let mut next = 2;
    if status.is_some() {
        from_where.push_str(&format!(" AND a.status = ${next}"));
        next += 1;
    }
    if starts_after.is_some() {
        from_where.push_str(&format!(
            " AND COALESCE(a.proposed_starts_at, s.starts_at) >= ${next}"
        ));
        next += 1;
    }
    if starts_before.is_some() {
        from_where.push_str(&format!(
            " AND COALESCE(a.proposed_starts_at, s.starts_at) < ${next}"
        ));
        next += 1;
    }
    if teacher.is_some() {
        from_where.push_str(&format!(" AND s.teacher = ${next}"));
    }
    // Close the derived table when the read is the joined shape; the plain
    // single-table shape opened none.
    if joined {
        from_where.push(')');
    }
    let mut list = PagedList::new(from_where, "ORDER BY id DESC").bind(requester.uuid());
    if let Some(status) = status {
        list = list.bind(status.as_str().to_string());
    }
    if let Some(after) = starts_after {
        list = list.bind(after.as_millis());
    }
    if let Some(before) = starts_before {
        list = list.bind(before.as_millis());
    }
    if let Some(teacher) = teacher {
        list = list.bind(teacher.uuid());
    }
    list.run(limit, offset, db).await
}

/// Every booking aimed at `teacher`, across all their slots — the
/// teacher's request inbox. The teacher lives on the slot row now, so
/// this is a join; the page and its count share the one predicate, and
/// the columns are spelled out so the join's duplicate slot-column names
/// can never leak into the row decode. `status` narrows to one booking
/// state and `starts_after`/`starts_before` to meetings whose effective
/// start (a standing proposal, the slot's window otherwise) sits in
/// `[after, before)` — both bounds optional, both in the SQL, so the
/// count always describes exactly the rows the window pages over.
pub async fn list_for_teacher(
    db: &Database,
    teacher: &UserId,
    status: Option<AppointmentStatus>,
    starts_after: Option<Timestamp>,
    starts_before: Option<Timestamp>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Appointment>, i64), AppError> {
    // The join wrapped as one derived table, so [`PagedList`]'s `SELECT *`
    // still sees exactly the appointment columns.
    let mut from_where = String::from(
        "(SELECT a.id, a.slot, a.requester, a.status, a.reason, \
         a.proposed_starts_at, a.proposed_ends_at, a.proposed_by, a.decided_by, \
         a.cancelled_by, a.cancel_reason, a.reject_reason, a.created_at \
         FROM appointment a JOIN appointment_slot s ON a.slot = s.id \
         WHERE s.teacher = $1",
    );
    let mut next = 2;
    if status.is_some() {
        from_where.push_str(&format!(" AND a.status = ${next}"));
        next += 1;
    }
    if starts_after.is_some() {
        from_where.push_str(&format!(
            " AND COALESCE(a.proposed_starts_at, s.starts_at) >= ${next}"
        ));
        next += 1;
    }
    if starts_before.is_some() {
        from_where.push_str(&format!(
            " AND COALESCE(a.proposed_starts_at, s.starts_at) < ${next}"
        ));
    }
    // Close the derived table the clauses were appended into.
    from_where.push(')');
    let mut list = PagedList::new(from_where, "ORDER BY id DESC").bind(teacher.uuid());
    if let Some(status) = status {
        list = list.bind(status.as_str().to_string());
    }
    if let Some(after) = starts_after {
        list = list.bind(after.as_millis());
    }
    if let Some(before) = starts_before {
        list = list.bind(before.as_millis());
    }
    list.run(limit, offset, db).await
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
pub(crate) async fn claim_slot_and_create(
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
    // Owned capture (`Send` rule of `tx_with_retry` closures).
    let expected = expected.clone();
    tx_with_retry(db, false, async move |tx| {
        decision_cas(&mut *tx, &expected, new.clone()).await
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
