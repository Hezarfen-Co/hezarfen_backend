//! Who actually ate. One row per (menu, student), keyed by the natural
//! composite primary key — the same pair always maps to the same row, so
//! marking is a single atomic upsert with no find-then-insert race, and
//! re-marking flips the status instead of stacking a second row.
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

use sqlx::Type;

use crate::constant::MEAL_ATTENDANCE_STATUSES;
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

/// The (menu, student) pair — the table's natural composite primary key. The
/// same trick as
/// [`MealBookingId`](crate::domain::meal_booking::MealBookingId). UUID strings
/// carry only `-`, so `_` is an unambiguous joiner — and the student half is
/// last, so the menu key (a `{date}_{slot}` pair, itself underscored) reads
/// back whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MealAttendanceId {
    menu: MenuId,
    student: UserId,
}

impl MealAttendanceId {
    pub fn composite(menu: &MenuId, student: &UserId) -> Self {
        Self {
            menu: menu.clone(),
            student: *student,
        }
    }

    /// Parse the `{menu}_{student}` wire form. A key that parses as no pair
    /// reads as the nil pair, which matches no row — exactly the 404 a
    /// dangling composite key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        let (menu, student) = key.rsplit_once('_').unwrap_or(("", ""));
        Self {
            menu: MenuId::from_key(menu),
            student: UserId::from_key(student),
        }
    }

    /// The `{menu}_{student}` wire form.
    pub fn key(&self) -> String {
        format!("{}_{}", self.menu.key(), self.student.key())
    }

    pub fn menu(&self) -> &MenuId {
        &self.menu
    }

    pub fn student(&self) -> UserId {
        self.student
    }
}

/// `served` or `missed`, and nothing else — see the module header.
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MealAttendance {
    pub(crate) menu: MenuId,
    pub(crate) student: UserId,
    pub(crate) status: MealAttendanceStatus,
    pub(crate) marked_by: UserId,
    pub(crate) marked_at: Timestamp,
}

impl MealAttendance {
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
