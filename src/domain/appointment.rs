//! A booking on a teacher's [`AppointmentSlot`]. A student or a parent asks
//! for the meeting with a reason; the teacher approves, rejects, or
//! counter-proposes another time.
//!
//! This module is the pure row shape: the validated reason and status, the
//! id, and the two predicates every layer shares — [`Appointment::window`]
//! (a proposal overrides the slot) and [`Appointment::overlaps`] (half-open
//! windows, so back-to-back meetings do not collide).
//!
//! The *cross-record* invariants live one layer down:
//!
//! - **One live booking per slot** — the `occupied` counter on the slot row,
//!   a [`cap`](crate::db::cap) of one. [`crate::service::appointment::book`]
//!   claims it *with* the booking row; rejecting or cancelling gives it back
//!   in the *same transaction* as the status flip
//!   ([`crate::db::appointment::save_if_unchanged`]), so the slot frees itself
//!   with nothing to sweep.
//! - **No double-booked teacher window.** Two overlapping published windows
//!   cannot both exist — the `appointment_slot` exclusion constraint refuses
//!   the second insert at the store, which is what replaced the
//!   [`crate::service::appointment::APPOINTMENT_LOCK`] serialization.
//!
//! The *single-row* invariant — a decision must be written onto the state it
//! was validated against — is a compare-and-set
//! ([`crate::db::appointment::save_if_unchanged`]), decided by a conditional
//! `UPDATE`.

use crate::constant::{MAX_APPOINTMENT_REASON_LEN};
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct AppointmentId(uuid::Uuid);

impl AppointmentId {
    /// Mints in write order — the `id` tie-break of the newest-first listings
    /// ([`crate::db::appointment::list_for_requester`]) must not scramble rows
    /// minted inside one millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row — a malformed path param stays a 404, exactly
    /// like a well-formed one that names nothing.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// Where a booking stands. Stored as the bare lowercase string the `status`
/// column's CHECK allows; an unknown value comes back as a decode error,
/// never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
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
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

/// Fields are crate-visible: [`crate::db::appointment`] reads the columns its
/// compare-and-set discriminates on, and [`crate::service::appointment`]
/// clones a fresh read and mutates the decision onto it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Appointment {
    pub(crate) id: AppointmentId,
    pub(crate) slot: AppointmentSlotId,
    pub(crate) requester: UserId,
    pub(crate) status: AppointmentStatus,
    pub(crate) reason: AppointmentReason,
    /// A counter-proposal from the teacher (or a new time the requester asks
    /// for). Set as a triple or not at all; once accepted it *is* the
    /// meeting's time, overriding the slot's own window.
    pub(crate) proposed_starts_at: Option<Timestamp>,
    pub(crate) proposed_ends_at: Option<Timestamp>,
    pub(crate) proposed_by: Option<UserId>,
    pub(crate) decided_by: Option<UserId>,
    /// Who called the meeting off, and why, once it is `cancelled`.
    pub(crate) cancelled_by: Option<UserId>,
    pub(crate) cancel_reason: Option<AppointmentReason>,
    /// Why the request was turned down; the rejecter is already on `decided_by`.
    pub(crate) reject_reason: Option<AppointmentReason>,
    pub(crate) created_at: Timestamp,
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

    /// Is a counter-proposal standing? Both halves, exactly as [`window`](Self::window)
    /// prefers them: a half-written triple is not a proposal and the slot's own
    /// window still rules.
    pub(crate) fn has_proposal(&self) -> bool {
        self.proposed_starts_at.is_some() && self.proposed_ends_at.is_some()
    }

    /// Is the standing proposal exactly this window? Milliseconds, the one form
    /// both the API and the row speak.
    pub(crate) fn proposes(&self, (starts_at, ends_at): (Timestamp, Timestamp)) -> bool {
        self.proposed_starts_at == Some(starts_at) && self.proposed_ends_at == Some(ends_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    #[tokio::test]
    async fn reason_is_required_and_capped() {
        assert!(AppointmentReason::try_new("").is_err());
        assert!(AppointmentReason::try_new("   ").is_err());
        assert!(AppointmentReason::try_new(&"x".repeat(MAX_APPOINTMENT_REASON_LEN)).is_ok());
        assert!(AppointmentReason::try_new(&"x".repeat(MAX_APPOINTMENT_REASON_LEN + 1)).is_err());
    }

    /// The `status` column is `TEXT` with a CHECK listing exactly these
    /// spellings: the occupancy query is written against them, so the storage
    /// form may not drift from `as_str`.
    #[test]
    fn status_stores_as_a_bare_string() {
        for (status, spelling) in [
            (AppointmentStatus::Pending, "pending"),
            (AppointmentStatus::Approved, "approved"),
            (AppointmentStatus::Rejected, "rejected"),
            (AppointmentStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.as_str(), spelling);
        }
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
}
