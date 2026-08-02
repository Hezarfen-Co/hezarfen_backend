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

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MEAL_ATTENDANCE_STATUSES, MEAL_ATTENDANCE_TABLE};
use crate::database::Database;
use crate::domain::menu::{MenuDate, MenuId};
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

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
    id: MealAttendanceId,
    menu: MenuId,
    student: UserId,
    status: MealAttendanceStatus,
    marked_by: UserId,
    marked_at: Timestamp,
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

    /// Record (or overwrite) whether `student` ate off `menu`. One row per
    /// pair by construction, so a correction is the same UPSERT flipping the
    /// status. No ledger line, ever — see the module header.
    ///
    /// `None` = the menu is gone, and the mark was *not* written. The menu's
    /// existence is the `UPSERT`'s own target rather than a read the write then
    /// trusts: the id list comes out of a `SELECT` over the menu row, so a menu
    /// deleted a moment ago yields no target and the statement writes nothing.
    /// An unconditional upsert landing just after `DELETE /meals/menus/{id}`
    /// committed left a mark its cascade had already swept — and since
    /// [`MenuId::for_slot`] is deterministic, republishing that day and slot
    /// resurrected it as a mark on the new menu.
    pub async fn mark(
        menu: &MenuId,
        student: &UserId,
        status: MealAttendanceStatus,
        marked_by: &UserId,
        db: &Database,
    ) -> Result<Option<MealAttendance>, AppError> {
        let row = MealAttendance {
            id: MealAttendanceId::composite(menu, student),
            menu: menu.clone(),
            student: student.clone(),
            status,
            marked_by: marked_by.clone(),
            marked_at: Timestamp::now(),
        };
        let mut result = db
            .query("UPSERT (SELECT VALUE $id FROM $menu) CONTENT $row RETURN AFTER")
            .bind(("id", row.id.record()))
            .bind(("menu", menu.record()))
            .bind(("row", row))
            .await?
            .check()?;
        Ok(result.take::<Vec<MealAttendance>>(0)?.into_iter().next())
    }

    /// The kitchen's list for one menu.
    pub async fn list_for_menu(
        menu: &MenuId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<MealAttendance>, i64), AppError> {
        PagedList::new(
            "meal_attendance WHERE menu = $menu",
            "ORDER BY marked_at DESC, id DESC",
        )
        .bind("menu", menu.record())
        .run(limit, offset, db)
        .await
    }

    /// One student's marks, newest first. `from`/`to` are inclusive
    /// `YYYY-MM-DD` bounds on the *menu's* day — the comparison is lexical,
    /// which is chronological for that format (see [`MenuDate`]), and the link
    /// is followed rather than denormalized so a row can never disagree with
    /// the menu it belongs to.
    pub async fn list_for_student(
        student: &UserId,
        from: Option<&MenuDate>,
        to: Option<&MenuDate>,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<MealAttendance>, i64), AppError> {
        PagedList::new(
            "meal_attendance WHERE student = $usr \
             AND ($from = NONE OR menu.date >= $from) \
             AND ($to = NONE OR menu.date <= $to)",
            "ORDER BY marked_at DESC, id DESC",
        )
        .bind("usr", student.record())
        .bind("from", from.map(|date| date.as_str().to_string()))
        .bind("to", to.map(|date| date.as_str().to_string()))
        .run(limit, offset, db)
        .await
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
