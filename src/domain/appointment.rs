//! A booking on a teacher's [`AppointmentSlot`]. A student or a parent asks
//! for the meeting with a reason; the teacher approves, rejects, or
//! counter-proposes another time.
//!
//! Two invariants live here, and both are guarded by [`APPOINTMENT_LOCK`]
//! rather than by the database:
//!
//! - **One live booking per slot.** Occupancy is counted from the rows, not
//!   stored — so rejecting or cancelling frees the slot with nothing to reset.
//! - **No double-booked person.** An approved meeting may not overlap another
//!   approved meeting of the same teacher *or* of the same requester.
//!
//! Both are count-then-write checks across records, which a `BEGIN…COMMIT`
//! does not serialize in SurrealDB (write-skew: two racing bookings each see a
//! free slot). This backend is the database's only writer, so one process-wide
//! lock closes it, exactly as `REGISTER_LOCK` does for event seats.

use std::sync::LazyLock;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;
use ulid::Generator;

use crate::constant::MAX_APPOINTMENT_REASON_LEN;
use crate::database::{APPOINTMENT_TABLE, Database};
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Serializes every occupancy/overlap decision: the check and the write it
/// authorizes must be one atomic step, and SurrealDB transactions do not
/// conflict-check a cross-record `count()` against a concurrent insert.
///
/// Lock order: no path ever holds this together with `ENROLL_LOCK`,
/// `TERM_LOCK`, `REGISTER_LOCK`, `EXAM_LOCK`, or `PRESENCE_LOCK` — appointments
/// touch no roster, term, event seat, or exam room, and none of those touch
/// appointments — so they cannot deadlock. Should a future path ever need two,
/// take this one *last*.
// ponytail: global lock; per-teacher locks if appointment traffic ever grows
// enough for the contention to matter.
pub(crate) static APPOINTMENT_LOCK: Mutex<()> = Mutex::const_new(());

/// Mints booking ids in write order — `Ulid::new()`'s random low bits sort
/// arbitrarily within one millisecond, which would scramble the `id` tie-break
/// of the newest-first listings below.
static IDS: LazyLock<std::sync::Mutex<Generator>> =
    LazyLock::new(|| std::sync::Mutex::new(Generator::new()));

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AppointmentId(RecordId);

impl AppointmentId {
    pub fn generate() -> Self {
        let mut ids = IDS.lock().expect("appointment id generator poisoned");
        // The only error is exhausting the random bits within one millisecond
        // (2^80 ids deep); it clears itself as the clock ticks, so retry.
        let ulid = loop {
            if let Ok(ulid) = ids.generate() {
                break ulid;
            }
        };
        Self(RecordId::new(APPOINTMENT_TABLE, ulid.to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(APPOINTMENT_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// Where a booking stands. `untagged` + `rename_all` store it as the bare
/// lowercase string the `status` column types as; an unknown value comes back
/// as a deserialization error, never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum AppointmentStatus {
    Pending,
    Approved,
    Rejected,
    Cancelled,
}

impl AppointmentStatus {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            AppointmentStatus::Pending => "pending",
            AppointmentStatus::Approved => "approved",
            AppointmentStatus::Rejected => "rejected",
            AppointmentStatus::Cancelled => "cancelled",
        }
    }

    /// Does a booking in this state still hold its slot? Rejected and
    /// cancelled ones do not — that is what frees the slot for re-booking.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            AppointmentStatus::Pending | AppointmentStatus::Approved
        )
    }
}

/// Why the requester wants the meeting. Required: a teacher approving a
/// parent-teacher conference needs to know what it is about.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AppointmentReason(String);

impl AppointmentReason {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("reason", value, MAX_APPOINTMENT_REASON_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The window an approved booking actually occupies, as read back from the
/// database: the counter-proposed time when there is one, the slot's own
/// otherwise. Optional fields because a dangling slot reference resolves to
/// `NONE` — such a row is skipped rather than failing the whole guard.
#[derive(Debug, Clone, SurrealValue)]
struct BookedWindow {
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Appointment {
    id: AppointmentId,
    slot: AppointmentSlotId,
    requester: UserId,
    status: AppointmentStatus,
    reason: AppointmentReason,
    /// A counter-proposal from the teacher (or a new time the requester asks
    /// for). Set as a triple or not at all; once accepted it *is* the
    /// meeting's time, overriding the slot's own window.
    proposed_starts_at: Option<Timestamp>,
    proposed_ends_at: Option<Timestamp>,
    proposed_by: Option<UserId>,
    decided_by: Option<UserId>,
    /// Who called the meeting off, and why, once it is `cancelled`. Both
    /// `#[surreal(default)]` so bookings written before cancellation records
    /// existed still read back (they resolve to `None`).
    #[surreal(default)]
    cancelled_by: Option<UserId>,
    #[surreal(default)]
    cancel_reason: Option<AppointmentReason>,
    /// Why the request was turned down; the rejecter is already on `decided_by`.
    #[surreal(default)]
    reject_reason: Option<AppointmentReason>,
    created_at: Timestamp,
}

impl Appointment {
    pub fn get_id(&self) -> &AppointmentId {
        &self.id
    }

    pub fn get_slot(&self) -> &AppointmentSlotId {
        &self.slot
    }

    pub fn get_requester(&self) -> &UserId {
        &self.requester
    }

    pub fn get_status(&self) -> AppointmentStatus {
        self.status
    }

    pub fn get_reason(&self) -> &AppointmentReason {
        &self.reason
    }

    pub fn get_proposed_starts_at(&self) -> Option<Timestamp> {
        self.proposed_starts_at
    }

    pub fn get_proposed_ends_at(&self) -> Option<Timestamp> {
        self.proposed_ends_at
    }

    pub fn get_proposed_by(&self) -> Option<&UserId> {
        self.proposed_by.as_ref()
    }

    pub fn get_decided_by(&self) -> Option<&UserId> {
        self.decided_by.as_ref()
    }

    pub fn get_cancelled_by(&self) -> Option<&UserId> {
        self.cancelled_by.as_ref()
    }

    pub fn get_cancel_reason(&self) -> Option<&AppointmentReason> {
        self.cancel_reason.as_ref()
    }

    pub fn get_reject_reason(&self) -> Option<&AppointmentReason> {
        self.reject_reason.as_ref()
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// When this booking actually happens: the proposed time if one is on the
    /// row, the slot's window otherwise. `slot` must be this booking's slot.
    pub fn window(&self, slot: &AppointmentSlot) -> (Timestamp, Timestamp) {
        match (self.proposed_starts_at, self.proposed_ends_at) {
            (Some(starts_at), Some(ends_at)) => (starts_at, ends_at),
            _ => (slot.get_starts_at(), slot.get_ends_at()),
        }
    }

    /// Do two half-open windows `[start, end)` collide? Touching windows
    /// (one ends exactly where the next begins) do **not** — back-to-back
    /// meetings are the normal way a teacher's hour is filled.
    pub fn overlaps(
        a_starts_at: Timestamp,
        a_ends_at: Timestamp,
        b_starts_at: Timestamp,
        b_ends_at: Timestamp,
    ) -> bool {
        a_starts_at.as_millis() < b_ends_at.as_millis()
            && a_ends_at.as_millis() > b_starts_at.as_millis()
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
    /// Caller must hold [`APPOINTMENT_LOCK`] for the answer to still be true
    /// by the time it is acted on.
    pub async fn conflicts(
        teacher: &UserId,
        requester: &UserId,
        starts_at: Timestamp,
        ends_at: Timestamp,
        except: Option<&AppointmentId>,
        db: &Database,
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
                Self::overlaps(starts_at, ends_at, booked_starts_at, booked_ends_at)
            }))
    }

    /// Does `slot` still hold a booking that keeps it occupied? The slot
    /// delete guard's question, and the re-booking gate's. Caller must hold
    /// [`APPOINTMENT_LOCK`].
    pub async fn has_live_booking(
        slot: &AppointmentSlotId,
        db: &Database,
    ) -> Result<bool, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM appointment WHERE slot = $slot \
                 AND status IN ['pending', 'approved'] LIMIT 1",
            )
            .bind(("slot", slot.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<Appointment>>(0)?.is_empty())
    }

    /// Request `slot`. Lands `pending`: publishing availability is not consent
    /// to a specific person and topic.
    ///
    /// Refused (409) when the slot's window has already opened, when it already
    /// carries a live booking, or when the requester is already committed
    /// elsewhere at that time — all re-derived
    /// from a fresh read under [`APPOINTMENT_LOCK`], so two racing requests
    /// cannot both find the slot free.
    pub async fn book(
        slot: &AppointmentSlotId,
        requester: &UserId,
        reason: AppointmentReason,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        let slot_row = AppointmentSlot::read(slot, db)
            .await?
            .ok_or(AppError::NotFound)?;
        if Self::has_live_booking(slot, db).await? {
            return Err(AppError::Conflict("the slot is already booked"));
        }
        // A window that has opened is history, not a plan: `cancel` refuses to
        // call off a meeting that started, so such a booking would be stuck
        // forever. `list_upcoming` hides these slots, but a stale id still
        // reaches here. The publish-time 60s skew grace does not apply.
        if slot_row.get_starts_at().as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("the slot has already started"));
        }
        // The teacher's own overlaps are not checked here — an approved
        // meeting of theirs elsewhere is the *approval*'s business, and
        // refusing the request outright would hide a slot they published.
        if Self::conflicts(
            requester,
            requester,
            slot_row.get_starts_at(),
            slot_row.get_ends_at(),
            None,
            db,
        )
        .await?
        {
            return Err(AppError::Conflict(
                "you already have an appointment at that time",
            ));
        }
        let appointment = Appointment {
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
        let created: Option<Appointment> = db
            .create(appointment.id.record())
            .content(appointment)
            .await?;
        created.ok_or_else(|| AppError::Internal("failed to book the appointment".into()))
    }

    pub async fn read(id: &AppointmentId, db: &Database) -> Result<Option<Appointment>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_for_requester(
        requester: &UserId,
        db: &Database,
    ) -> Result<Vec<Appointment>, AppError> {
        let mut result = db
            .query("SELECT * FROM appointment WHERE requester = $requester ORDER BY id DESC")
            .bind(("requester", requester.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Appointment>>(0)?)
    }

    /// Every booking aimed at `teacher`, across all their slots — the teacher's
    /// request inbox.
    pub async fn list_for_teacher(
        teacher: &UserId,
        db: &Database,
    ) -> Result<Vec<Appointment>, AppError> {
        let mut result = db
            .query("SELECT * FROM appointment WHERE slot.teacher = $teacher ORDER BY id DESC")
            .bind(("teacher", teacher.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Appointment>>(0)?)
    }

    pub async fn list_for_slot(
        slot: &AppointmentSlotId,
        db: &Database,
    ) -> Result<Vec<Appointment>, AppError> {
        let mut result = db
            .query("SELECT * FROM appointment WHERE slot = $slot ORDER BY id DESC")
            .bind(("slot", slot.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Appointment>>(0)?)
    }

    /// Confirm the meeting. Re-runs the double-booking guard for both teacher
    /// and requester against a fresh read under [`APPOINTMENT_LOCK`] — the
    /// authoritative check, since only approval commits anyone. Refused (409)
    /// when the effective window has already started; that also covers
    /// [`accept_proposal`], which lands here.
    pub async fn approve(
        id: &AppointmentId,
        decided_by: &UserId,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        let mut appointment = Self::read_pending(id, db).await?;
        let slot = AppointmentSlot::read(&appointment.slot, db)
            .await?
            .ok_or(AppError::NotFound)?;
        let (starts_at, ends_at) = appointment.window(&slot);
        // The effective window — the proposal's when one stands — must still be
        // ahead. Mutual agreement does not help: `cancel` refuses a meeting
        // that started, so committing to a passed window would mint a booking
        // nobody can ever undo. Refused, the row stays `pending` and the
        // teacher can propose a time that can actually happen.
        if starts_at.as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("that time has already started"));
        }
        if Self::conflicts(
            slot.get_teacher(),
            &appointment.requester,
            starts_at,
            ends_at,
            Some(id),
            db,
        )
        .await?
        {
            return Err(AppError::Conflict(
                "that time collides with another approved appointment",
            ));
        }
        appointment.status = AppointmentStatus::Approved;
        appointment.decided_by = Some(decided_by.clone());
        Self::save(appointment, db).await
    }

    /// Turn the request down. Frees the slot: occupancy counts live rows only.
    pub async fn reject(
        id: &AppointmentId,
        decided_by: &UserId,
        reason: Option<AppointmentReason>,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        let mut appointment = Self::read_pending(id, db).await?;
        appointment.status = AppointmentStatus::Rejected;
        appointment.decided_by = Some(decided_by.clone());
        appointment.reject_reason = reason;
        Self::save(appointment, db).await
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
        id: &AppointmentId,
        cancelled_by: &UserId,
        reason: Option<AppointmentReason>,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        let mut appointment = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
        if !appointment.status.is_live() {
            return Err(AppError::Conflict("the appointment is already settled"));
        }
        let slot = AppointmentSlot::read(&appointment.slot, db)
            .await?
            .ok_or(AppError::NotFound)?;
        if appointment.window(&slot).0.as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("the appointment has already started"));
        }
        appointment.status = AppointmentStatus::Cancelled;
        appointment.cancelled_by = Some(cancelled_by.clone());
        appointment.cancel_reason = reason;
        Self::save(appointment, db).await
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
        id: &AppointmentId,
        starts_at: Timestamp,
        ends_at: Timestamp,
        proposed_by: &UserId,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        if starts_at.as_millis() >= ends_at.as_millis() {
            return Err(ValidationError::Invalid {
                field: "proposed_ends_at",
                reason: "must be after proposed_starts_at",
            }
            .into());
        }
        let _guard = APPOINTMENT_LOCK.lock().await;
        let mut appointment = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
        if !appointment.status.is_live() {
            return Err(AppError::Conflict("the appointment is already settled"));
        }
        if starts_at.as_millis() <= Timestamp::now().as_millis() {
            return Err(AppError::Conflict("that time has already started"));
        }
        appointment.status = AppointmentStatus::Pending;
        appointment.decided_by = None;
        appointment.proposed_starts_at = Some(starts_at);
        appointment.proposed_ends_at = Some(ends_at);
        appointment.proposed_by = Some(proposed_by.clone());
        Self::save(appointment, db).await
    }

    /// Accept the standing proposal — approval at the proposed time. The
    /// overlap guard runs again because the time moved.
    pub async fn accept_proposal(
        id: &AppointmentId,
        decided_by: &UserId,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        {
            let appointment = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
            if appointment.proposed_starts_at.is_none() || appointment.proposed_ends_at.is_none() {
                return Err(AppError::Conflict("no time has been proposed"));
            }
        }
        // `approve` re-reads under the lock and reads the proposed window off
        // the row, so accepting is approval — no second code path.
        Self::approve(id, decided_by, db).await
    }

    /// The row, insisting it is still open for a decision.
    async fn read_pending(id: &AppointmentId, db: &Database) -> Result<Appointment, AppError> {
        let appointment = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
        if appointment.status != AppointmentStatus::Pending {
            return Err(AppError::Conflict("the appointment is no longer pending"));
        }
        Ok(appointment)
    }

    async fn save(appointment: Appointment, db: &Database) -> Result<Appointment, AppError> {
        let updated: Option<Appointment> = db
            .update(appointment.id.record())
            .content(appointment)
            .await?;
        updated.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Value;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    /// A window that has not opened yet — `book` refuses started ones.
    fn soon(offset: i64) -> Timestamp {
        at(Timestamp::now().as_millis() + offset)
    }

    #[tokio::test]
    async fn reason_is_required_and_capped() {
        assert!(AppointmentReason::try_new("").is_err());
        assert!(AppointmentReason::try_new("   ").is_err());
        assert!(AppointmentReason::try_new(&"x".repeat(MAX_APPOINTMENT_REASON_LEN)).is_ok());
        assert!(AppointmentReason::try_new(&"x".repeat(MAX_APPOINTMENT_REASON_LEN + 1)).is_err());
    }

    /// The `status` column is `TYPE string`: an object-wrapped enum would be
    /// rejected on write, and the `status IN ['pending','approved']` occupancy
    /// query is written against exactly these spellings.
    #[tokio::test]
    async fn status_stores_as_a_bare_string() {
        for status in [
            AppointmentStatus::Pending,
            AppointmentStatus::Approved,
            AppointmentStatus::Rejected,
            AppointmentStatus::Cancelled,
        ] {
            let value = status.into_value();
            assert_eq!(value, Value::String(status.as_str().to_string()));
            assert_eq!(AppointmentStatus::from_value(value).unwrap(), status);
        }
        assert!(AppointmentStatus::from_value(Value::String("declined".into())).is_err());
        assert!(AppointmentStatus::Pending.is_live());
        assert!(AppointmentStatus::Approved.is_live());
        assert!(!AppointmentStatus::Rejected.is_live());
        assert!(!AppointmentStatus::Cancelled.is_live());
    }

    #[tokio::test]
    async fn touching_windows_do_not_overlap_but_one_millisecond_does() {
        // 10:00–10:30 then 10:30–11:00: back to back, no conflict.
        assert!(!Appointment::overlaps(at(0), at(30), at(30), at(60)));
        assert!(!Appointment::overlaps(at(30), at(60), at(0), at(30)));
        // One millisecond of genuine overlap, from either side.
        assert!(Appointment::overlaps(at(0), at(31), at(30), at(60)));
        assert!(Appointment::overlaps(at(30), at(60), at(0), at(31)));
        // Containment, both ways, and an exact match.
        assert!(Appointment::overlaps(at(10), at(20), at(0), at(60)));
        assert!(Appointment::overlaps(at(0), at(60), at(10), at(20)));
        assert!(Appointment::overlaps(at(0), at(30), at(0), at(30)));
        // Disjoint by a whole hour.
        assert!(!Appointment::overlaps(at(0), at(30), at(90), at(120)));
    }

    #[tokio::test]
    async fn window_prefers_a_proposal_over_the_slot() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let (starts_at, ends_at) = (soon(60_000), soon(120_000));
        let slot = AppointmentSlot::create(&teacher, starts_at, ends_at, None, &db)
            .await
            .unwrap();
        let mut appointment = Appointment::book(
            slot.get_id(),
            &UserId::from_key("s1"),
            AppointmentReason::try_new("ödev").unwrap(),
            &db,
        )
        .await
        .unwrap();

        assert_eq!(appointment.window(&slot), (starts_at, ends_at));
        appointment.proposed_starts_at = Some(at(5_000));
        appointment.proposed_ends_at = Some(at(6_000));
        assert_eq!(appointment.window(&slot), (at(5_000), at(6_000)));
    }

    /// A slot whose window already opened is unbookable: `cancel` would refuse
    /// to undo the resulting meeting, so the booking would be stuck for good.
    #[tokio::test]
    async fn a_started_slot_cannot_be_booked() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let reason = || AppointmentReason::try_new("görüşme").unwrap();
        let started = AppointmentSlot::create(&teacher, soon(-30_000), soon(60_000), None, &db)
            .await
            .unwrap();
        assert!(matches!(
            Appointment::book(started.get_id(), &UserId::from_key("s1"), reason(), &db).await,
            Err(AppError::Conflict("the slot has already started"))
        ));

        let upcoming = AppointmentSlot::create(&teacher, soon(60_000), soon(120_000), None, &db)
            .await
            .unwrap();
        assert!(
            Appointment::book(upcoming.get_id(), &UserId::from_key("s1"), reason(), &db)
                .await
                .is_ok()
        );
    }

    /// Approval judges the *effective* window, so a proposal whose time has
    /// come and gone cannot be accepted — the meeting would be uncancellable.
    #[tokio::test]
    async fn a_started_proposal_cannot_be_accepted() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let student = UserId::from_key("s1");
        let slot = AppointmentSlot::create(&teacher, soon(60_000), soon(120_000), None, &db)
            .await
            .unwrap();
        let booking = Appointment::book(
            slot.get_id(),
            &student,
            AppointmentReason::try_new("görüşme").unwrap(),
            &db,
        )
        .await
        .unwrap();

        // Proposing a window that already opened is refused up front — the
        // handler's 60s skew grace would otherwise let one through.
        assert!(matches!(
            Appointment::propose(booking.get_id(), soon(-30_000), soon(30_000), &teacher, &db)
                .await,
            Err(AppError::Conflict("that time has already started"))
        ));
        // And approval judges the effective window again, for a proposal whose
        // time simply passed while it stood: forced onto the row directly,
        // since `propose` no longer mints one.
        let mut standing = Appointment::read(booking.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        standing.proposed_starts_at = Some(soon(-30_000));
        standing.proposed_ends_at = Some(soon(30_000));
        Appointment::save(standing, &db).await.unwrap();
        assert!(matches!(
            Appointment::accept_proposal(booking.get_id(), &student, &db).await,
            Err(AppError::Conflict("that time has already started"))
        ));
        // Refused, not half-applied: still pending, still decidable at a time
        // that can actually happen.
        let after = Appointment::read(booking.get_id(), &db).await.unwrap();
        assert_eq!(after.unwrap().get_status(), AppointmentStatus::Pending);
        Appointment::propose(booking.get_id(), soon(60_000), soon(120_000), &teacher, &db)
            .await
            .unwrap();
        let accepted = Appointment::accept_proposal(booking.get_id(), &student, &db)
            .await
            .unwrap();
        assert_eq!(accepted.get_status(), AppointmentStatus::Approved);
    }

    /// Occupancy is derived, so a rejected booking must hand the slot back.
    #[tokio::test]
    async fn rejecting_frees_the_slot_for_re_booking() {
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slot = AppointmentSlot::create(&teacher, soon(60_000), soon(120_000), None, &db)
            .await
            .unwrap();
        let reason = || AppointmentReason::try_new("görüşme").unwrap();

        let first = Appointment::book(slot.get_id(), &UserId::from_key("s1"), reason(), &db)
            .await
            .unwrap();
        assert!(matches!(
            Appointment::book(slot.get_id(), &UserId::from_key("s2"), reason(), &db).await,
            Err(AppError::Conflict(_))
        ));
        // The live booking also blocks the slot delete.
        assert!(matches!(
            slot.clone().delete(&db).await,
            Err(AppError::Conflict(_))
        ));

        Appointment::reject(first.get_id(), &teacher, None, &db)
            .await
            .unwrap();
        let second = Appointment::book(slot.get_id(), &UserId::from_key("s2"), reason(), &db)
            .await
            .unwrap();
        assert_eq!(second.get_status(), AppointmentStatus::Pending);
    }
}
