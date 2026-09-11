//! Who actually ate. One row per (menu, student), keyed by a deterministic
//! composite id — the same pair always maps to the same record, so marking is a
//! single atomic UPSERT with no find-then-insert race, and re-marking flips the
//! status instead of stacking a second row.
//!
//! **Attendance is reporting only — it has zero billing effect.** Booking is
//! the sole charge trigger: a student who booked and did not eat still pays,
//! because the kitchen bought the food. Nothing here ever writes to
//! [`MealLedger`](crate::domain::meal_ledger::MealLedger) — no no-show penalty,
//! no refund-on-missed, no auto-reversal. A walk-in with no booking can still
//! be marked `served` (the record is operationally true) and is likewise never
//! charged for it.
//!
//! The statuses are the fixed [`MEAL_ATTENDANCE_STATUSES`] pair, deliberately
//! *not* the school's editable `attendance_statuses`: a canteen line has no
//! "late" or "excused".
//!
//! The queries live in [`crate::db::meal_attendance`]; the web layer reads
//! through [`crate::service::meal_attendance`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MEAL_ATTENDANCE_STATUSES, MEAL_ATTENDANCE_TABLE};
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MealAttendanceId(RecordId);

impl MealAttendanceId {
    /// A deterministic id for the (menu, student) pair — same trick as
    /// [`MealBookingId`](crate::domain::meal_booking::MealBookingId). ULID keys
    /// are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(menu: &MenuId, student: &UserId) -> Self {
        Self(RecordId::new(
            MEAL_ATTENDANCE_TABLE,
            format!("{}_{}", menu.key(), student.key()),
        ))
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

/// `served` or `missed`, and nothing else — see the module header.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MealAttendanceStatus(String);

impl MealAttendanceStatus {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        if !MEAL_ATTENDANCE_STATUSES.contains(&value) {
            return Err(ValidationError::Invalid {
                field: "status",
                reason: "must be 'served' or 'missed'",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct MealAttendance {
    pub(crate) id: MealAttendanceId,
    pub(crate) menu: MenuId,
    pub(crate) student: UserId,
    pub(crate) status: MealAttendanceStatus,
    pub(crate) marked_by: UserId,
    pub(crate) marked_at: Timestamp,
}

impl MealAttendance {
    pub fn get_id(&self) -> &MealAttendanceId {
        &self.id
    }

    pub fn get_menu(&self) -> &MenuId {
        &self.menu
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_status(&self) -> &MealAttendanceStatus {
        &self.status
    }

    pub fn get_marked_by(&self) -> &UserId {
        &self.marked_by
    }

    pub fn get_marked_at(&self) -> Timestamp {
        self.marked_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_the_fixed_canteen_pair() {
        for status in MEAL_ATTENDANCE_STATUSES {
            assert_eq!(
                MealAttendanceStatus::try_new(status).unwrap().as_str(),
                status
            );
        }
        // The school's roll-call vocabulary is not this vocabulary.
        assert!(MealAttendanceStatus::try_new("present").is_err());
        assert!(MealAttendanceStatus::try_new("excused").is_err());
        assert!(MealAttendanceStatus::try_new("").is_err());
    }
}
