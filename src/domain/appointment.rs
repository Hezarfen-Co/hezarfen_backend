//! A booking on a teacher's [`AppointmentSlot`]. A student or a parent asks
//! for the meeting with a reason; the teacher approves, rejects, or
//! counter-proposes another time.
//!
//! Two *cross-record* invariants live here, guarded very differently:
//!
//! - **One live booking per slot** — the `occupied` counter on the slot row, a
//!   [`cap`](crate::domain::cap) of one. Booking claims it *with* the booking
//!   row ([`claim_and_create`](cap::claim_and_create));
//!   rejecting or cancelling gives it back in the *same transaction* as the
//!   status flip, so the slot frees itself with nothing to sweep. Both are
//!   single-record conditional writes, so the store decides it, not a lock.
//! - **No double-booked person.** An approved meeting may not overlap another
//!   approved meeting of the same teacher *or* of the same requester. That is a
//!   count-then-write over *many* rows, which a `BEGIN…COMMIT` does not
//!   serialize in SurrealDB (write-skew), so it stays under
//!   [`APPOINTMENT_LOCK`], which is the whole guarantee behind it.
//!
//! The *single-row* invariant — a decision must be written onto the state it
//! was validated against — is **not** the lock's job, and never was: a
//! whole-row save built on a snapshot read before the lock was taken would
//! silently drop whatever landed in between. That one is a compare-and-set
//! ([`Appointment::save_if_unchanged`]), decided by the store.
//!
//! Overlap therefore rests on the lock alone — it is a predicate over *other*
//! rows, with no single row to key a counter or a CAS on. That holds while
//! every approving write takes [`APPOINTMENT_LOCK`]; one that skips it puts two
//! meetings in one half-hour, so the rule is stated here rather than left to be
//! noticed. The damage if it ever happens is one double-booked half-hour,
//! visible to both parties and fixable by cancelling either side — no money, no
//! grade, no data loss.

use std::sync::LazyLock;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;
use ulid::Generator;

use crate::constant::{
    APPOINTMENT_TABLE, CAS_UPDATE_RETRIES, MAX_APPOINTMENT_REASON_LEN, SLOT_OCCUPIED_FIELD,
};
use crate::database::{Database, lost_the_race};
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId};
use crate::domain::cap;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Serializes the **overlap** check and the write it authorizes — nothing else.
/// "Is this teacher (or this requester) already committed at that time" is a
/// predicate over other appointment rows, and SurrealDB does not conflict-check
/// a cross-record read against a concurrent insert, so the check and its write
/// have to be one atomic step, and this lock is the only thing that makes them
/// one. Occupancy is no longer its business: that is the slot's `occupied`
/// counter, a conditional single-record write the store decides.
///
/// So this is load-bearing, not a convenience: an approval path that does not
/// take it can double-book a teacher, as the module doc records.
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
        slot: &AppointmentSlotId,
        requester: &UserId,
        reason: AppointmentReason,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        let slot_row = AppointmentSlot::read(slot, db)
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
        // Last, so nothing above can refuse *after* the slot was taken. A slot
        // deleted since the read above is gone, not full — the claim's `WHERE`
        // finds no row either way, so the distinction is re-read rather than
        // guessed.
        let seat = slot.record();
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
        match cap::claim_and_create(
            &seat,
            SLOT_OCCUPIED_FIELD,
            1,
            &appointment.id.record(),
            &appointment,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => Ok(created),
            cap::Claimed::Full => {
                if AppointmentSlot::read(slot, db).await?.is_none() {
                    return Err(AppError::NotFound);
                }
                Err(AppError::Conflict("the slot is already booked"))
            }
            // The id is a freshly minted ULID on a table with no UNIQUE index,
            // so no rival can have aimed at it.
            cap::Claimed::Duplicate => Err(AppError::Internal("appointment id collided".into())),
        }
    }

    pub async fn read(id: &AppointmentId, db: &Database) -> Result<Option<Appointment>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_for_requester(
        requester: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
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
        teacher: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
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
        id: &AppointmentId,
        decided_by: &UserId,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        Self::approve_inner(id, decided_by, None, db).await
    }

    /// The one approval path. `accepting` carries the window the caller is
    /// answering *for*, which is the only way a proposed one may be committed:
    /// it both marks the caller as the proposal's counterparty and pins which
    /// proposal they saw. Re-checked on every retry round, so a proposal that
    /// moved between the read and the write is refused rather than confirmed.
    async fn approve_inner(
        id: &AppointmentId,
        decided_by: &UserId,
        accepting: Option<(Timestamp, Timestamp)>,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        let _guard = APPOINTMENT_LOCK.lock().await;
        for _ in 0..CAS_UPDATE_RETRIES {
            let expected = Self::read_pending(id, db).await?;
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
            let slot = AppointmentSlot::read(&expected.slot, db)
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
            if Self::conflicts(
                slot.get_teacher(),
                &expected.requester,
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
            let mut approved = expected.clone();
            approved.status = AppointmentStatus::Approved;
            approved.decided_by = Some(decided_by.clone());
            if let Some(saved) = approved.save_if_unchanged(&expected, db).await? {
                return Ok(saved);
            }
        }
        Err(contended())
    }

    /// Turn the request down. Frees the slot — the seat goes back with the
    /// status flip, in one transaction. No lock: this decides nothing about
    /// anyone's calendar, and the write is its own guard.
    pub async fn reject(
        id: &AppointmentId,
        decided_by: &UserId,
        reason: Option<AppointmentReason>,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        for _ in 0..CAS_UPDATE_RETRIES {
            let expected = Self::read_pending(id, db).await?;
            let mut rejected = expected.clone();
            rejected.status = AppointmentStatus::Rejected;
            rejected.decided_by = Some(decided_by.clone());
            rejected.reject_reason = reason.clone();
            if let Some(saved) = rejected.save_if_unchanged(&expected, db).await? {
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
        id: &AppointmentId,
        cancelled_by: &UserId,
        reason: Option<AppointmentReason>,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        for _ in 0..CAS_UPDATE_RETRIES {
            let expected = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
            if !expected.status.is_live() {
                return Err(AppError::Conflict("the appointment is already settled"));
            }
            let slot = AppointmentSlot::read(&expected.slot, db)
                .await?
                .ok_or(AppError::NotFound)?;
            if expected.window(&slot).0.as_millis() <= Timestamp::now().as_millis() {
                return Err(AppError::Conflict("the appointment has already started"));
            }
            let mut cancelled = expected.clone();
            cancelled.status = AppointmentStatus::Cancelled;
            cancelled.cancelled_by = Some(cancelled_by.clone());
            cancelled.cancel_reason = reason.clone();
            if let Some(saved) = cancelled.save_if_unchanged(&expected, db).await? {
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
        // No lock: a proposal commits nobody's calendar (the booking goes back
        // to `pending`), and it neither takes nor frees the slot's seat.
        for _ in 0..CAS_UPDATE_RETRIES {
            let expected = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
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
            if let Some(saved) = proposed.save_if_unchanged(&expected, db).await? {
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
        id: &AppointmentId,
        decided_by: &UserId,
        starts_at: Timestamp,
        ends_at: Timestamp,
        db: &Database,
    ) -> Result<Appointment, AppError> {
        // The same approval path re-reads under the lock and reads the proposed
        // window off the row, so accepting is approval — no second code path.
        // Only this door passes `accepting`, and the web layer opens it to the
        // requester alone.
        Self::approve_inner(id, decided_by, Some((starts_at, ends_at)), db).await
    }

    /// Is a counter-proposal standing? Both halves, exactly as [`window`](Self::window)
    /// prefers them: a half-written triple is not a proposal and the slot's own
    /// window still rules.
    fn has_proposal(&self) -> bool {
        self.proposed_starts_at.is_some() && self.proposed_ends_at.is_some()
    }

    /// Is the standing proposal exactly this window? Milliseconds, the one form
    /// both the API and the row speak.
    fn proposes(&self, (starts_at, ends_at): (Timestamp, Timestamp)) -> bool {
        self.proposed_starts_at == Some(starts_at) && self.proposed_ends_at == Some(ends_at)
    }

    /// The row, insisting it is still open for a decision.
    async fn read_pending(id: &AppointmentId, db: &Database) -> Result<Appointment, AppError> {
        let appointment = Self::read(id, db).await?.ok_or(AppError::NotFound)?;
        if appointment.status != AppointmentStatus::Pending {
            return Err(AppError::Conflict("the appointment is no longer pending"));
        }
        Ok(appointment)
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
    async fn save_if_unchanged(
        self,
        expected: &Appointment,
        db: &Database,
    ) -> Result<Option<Appointment>, AppError> {
        let frees_the_slot = expected.status.is_live() && !self.status.is_live();
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
                .bind(("new", self))
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
        let current = Appointment::read(booking.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        let (passed_starts_at, passed_ends_at) = (soon(-30_000), soon(30_000));
        let mut standing = current.clone();
        standing.proposed_starts_at = Some(passed_starts_at);
        standing.proposed_ends_at = Some(passed_ends_at);
        standing
            .save_if_unchanged(&current, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            Appointment::accept_proposal(
                booking.get_id(),
                &student,
                passed_starts_at,
                passed_ends_at,
                &db
            )
            .await,
            Err(AppError::Conflict("that time has already started"))
        ));
        // Refused, not half-applied: still pending, still decidable at a time
        // that can actually happen.
        let after = Appointment::read(booking.get_id(), &db).await.unwrap();
        assert_eq!(after.unwrap().get_status(), AppointmentStatus::Pending);
        let (starts_at, ends_at) = (soon(60_000), soon(120_000));
        Appointment::propose(booking.get_id(), starts_at, ends_at, &teacher, &db)
            .await
            .unwrap();
        // Accepting a window that is *not* the standing proposal is refused —
        // the pin is what the requester saw, not a request to move the meeting.
        assert!(matches!(
            Appointment::accept_proposal(
                booking.get_id(),
                &student,
                passed_starts_at,
                passed_ends_at,
                &db
            )
            .await,
            Err(AppError::Conflict(
                "the proposed time has changed; \
                 re-read the booking and accept the new one"
            ))
        ));
        let accepted =
            Appointment::accept_proposal(booking.get_id(), &student, starts_at, ends_at, &db)
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

        // One transaction counter-proposes. A second concurrent transaction
        // is still holding the snapshot it read before that — same id, same
        // `pending` status.
        let stale = booking.clone();
        let (proposed_starts_at, proposed_ends_at) = (soon(180_000), soon(240_000));
        Appointment::propose(
            booking.get_id(),
            proposed_starts_at,
            proposed_ends_at,
            &teacher,
            &db,
        )
        .await
        .unwrap();

        let mut late = stale.clone();
        late.status = AppointmentStatus::Approved;
        late.decided_by = Some(teacher.clone());
        assert!(late.save_if_unchanged(&stale, &db).await.unwrap().is_none());

        // The proposal survived: the row was not confirmed at the slot's old
        // window behind the requester's back.
        let after = Appointment::read(booking.get_id(), &db)
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
        let saved = fresh
            .save_if_unchanged(&after, &db)
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
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slot = AppointmentSlot::create(&teacher, soon(60_000), soon(120_000), None, &db)
            .await
            .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 0);

        Appointment::book(
            slot.get_id(),
            &UserId::from_key("s1"),
            AppointmentReason::try_new("görüşme").unwrap(),
            &db,
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
            Appointment::list_for_slot(slot.get_id(), &db)
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
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("t1");
        let slot = AppointmentSlot::create(&teacher, soon(60_000), soon(120_000), None, &db)
            .await
            .unwrap();
        let reason = || AppointmentReason::try_new("görüşme").unwrap();

        Appointment::book(slot.get_id(), &UserId::from_key("s1"), reason(), &db)
            .await
            .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 1);

        let refused =
            Appointment::book(slot.get_id(), &UserId::from_key("s2"), reason(), &db).await;
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
            Appointment::list_for_slot(slot.get_id(), &db)
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
        Appointment::approve(booking.get_id(), &teacher, &db)
            .await
            .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 1, "approval holds it");

        Appointment::cancel(booking.get_id(), &student, None, &db)
            .await
            .unwrap();
        assert_eq!(occupied(slot.get_id(), &db).await, 0);
        // Cancelling twice is refused, so the seat cannot go back twice either.
        assert!(
            Appointment::cancel(booking.get_id(), &student, None, &db)
                .await
                .is_err()
        );
        assert_eq!(occupied(slot.get_id(), &db).await, 0);
        assert!(
            Appointment::book(
                slot.get_id(),
                &UserId::from_key("s2"),
                AppointmentReason::try_new("görüşme").unwrap(),
                &db
            )
            .await
            .is_ok()
        );
        assert_eq!(occupied(slot.get_id(), &db).await, 1);
    }

    /// Occupancy frees on reject, so the slot can be re-booked.
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
        assert_eq!(occupied(slot.get_id(), &db).await, 0);
        let second = Appointment::book(slot.get_id(), &UserId::from_key("s2"), reason(), &db)
            .await
            .unwrap();
        assert_eq!(second.get_status(), AppointmentStatus::Pending);
        assert_eq!(occupied(slot.get_id(), &db).await, 1);
    }
}
