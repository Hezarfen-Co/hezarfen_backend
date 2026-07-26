//! A published menu: what the kitchen serves on one calendar day in one meal
//! slot. Dishes hang off it ([`MenuDish`](crate::domain::menu_dish::MenuDish)),
//! and bookings will hang off its `capacity`.
//!
//! Two things are deliberate:
//!
//! - **`date` is text** (`YYYY-MM-DD`), not a timestamp. The uniqueness rule is
//!   "one menu per day and slot", an equality test — and a midnight-in-millis
//!   day is only one school's day.
//! - **`slot` is a snapshot**, not a link into settings. Retiring a slot must
//!   never rewrite a menu already published under it.

use std::sync::LazyLock;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;
use ulid::Generator;

use crate::constant::{MAX_MENU_CAPACITY, MENU_TABLE};
use crate::database::Database;
use crate::domain::field_update::FieldUpdate;
use crate::domain::menu_dish::MenuDish;
use crate::domain::settings::MealSlotDef;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Serializes "is there already a menu for this day and slot?" against the
/// publish it authorizes — a count-then-write pair SurrealDB does not
/// conflict-check, so the UNIQUE index would otherwise be the only thing
/// standing between two racing publishes and a 500. Also held by the settings
/// slot-removal guard, whose check ("was any menu published for this slot?")
/// is the mirror image.
///
/// Lock order: `EXAM_LOCK` (taken by the same settings write) is always taken
/// *before* this one; nothing here ever takes `EXAM_LOCK`, so the two cannot
/// deadlock.
pub(crate) static MENU_LOCK: Mutex<()> = Mutex::const_new(());

/// Mints menu ids in write order — `Ulid::new()`'s random low bits sort
/// arbitrarily within one millisecond, which would scramble the `id` tie-break
/// of the listings below.
static IDS: LazyLock<std::sync::Mutex<Generator>> =
    LazyLock::new(|| std::sync::Mutex::new(Generator::new()));

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MenuId(RecordId);

impl MenuId {
    pub fn generate() -> Self {
        let mut ids = IDS.lock().expect("menu id generator poisoned");
        // The only error is exhausting the random bits within one millisecond
        // (2^80 ids deep); it clears itself as the clock ticks, so retry.
        let ulid = loop {
            if let Ok(ulid) = ids.generate() {
                break ulid;
            }
        };
        Self(RecordId::new(MENU_TABLE, ulid.to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(MENU_TABLE, key))
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

/// A calendar day as `YYYY-MM-DD`. Zero-padded and fixed-width on purpose:
/// that makes the text sort chronologically, so the `?from=&to=` range filter
/// and the newest-first ordering are plain string comparisons.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, SurrealValue)]
pub struct MenuDate(String);

impl MenuDate {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let invalid = ValidationError::Invalid {
            field: "date",
            reason: "must be a calendar day as YYYY-MM-DD",
        };
        let value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(invalid);
        }
        let digits =
            |from: usize, to: usize| -> Option<u32> { value.get(from..to)?.parse::<u32>().ok() };
        let (Some(_year), Some(month), Some(day)) = (digits(0, 4), digits(5, 7), digits(8, 10))
        else {
            return Err(invalid);
        };
        // Day count per month is not checked: the calendar rules (leap years)
        // would be the only thing this file knows about time, and a 31st of
        // February menu is a typo nobody can act on, not a data hazard.
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(invalid);
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The meal slot a menu was published for, snapshotted as text. Validated
/// against the school's *current* list at write time — that list is the only
/// place slot names are defined, and this is the sole funnel into the column.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MenuSlot(String);

impl MenuSlot {
    pub fn try_new(value: &str, allowed: &[MealSlotDef]) -> Result<Self, ValidationError> {
        let value = value.trim();
        if !allowed.iter().any(|slot| slot.get_name() == value) {
            return Err(ValidationError::Invalid {
                field: "slot",
                reason: "not one of the school's meal slots (see GET /settings)",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How many students the menu may seat. `None` = uncapped, the shape a
/// course's `capacity` already uses.
pub fn validate_capacity(value: Option<i64>) -> Result<(), ValidationError> {
    match value {
        Some(capacity) if !(0..=MAX_MENU_CAPACITY).contains(&capacity) => {
            Err(ValidationError::Invalid {
                field: "capacity",
                reason: "must be between 0 and 10000",
            })
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Menu {
    id: MenuId,
    date: MenuDate,
    slot: MenuSlot,
    capacity: Option<i64>,
    created_by: UserId,
    created_at: Timestamp,
}

impl Menu {
    pub fn get_id(&self) -> &MenuId {
        &self.id
    }

    pub fn get_date(&self) -> &MenuDate {
        &self.date
    }

    pub fn get_slot(&self) -> &MenuSlot {
        &self.slot
    }

    pub fn get_capacity(&self) -> Option<i64> {
        self.capacity
    }

    pub fn get_created_by(&self) -> &UserId {
        &self.created_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Publish a menu. Refused (409) when the day+slot already carries one —
    /// re-checked under [`MENU_LOCK`], so two racing publishes cannot both find
    /// the day free and trip the UNIQUE index into a 500.
    pub async fn create(
        date: MenuDate,
        slot: MenuSlot,
        capacity: Option<i64>,
        created_by: &UserId,
        db: &Database,
    ) -> Result<Menu, AppError> {
        let _guard = MENU_LOCK.lock().await;
        if Self::find(&date, &slot, db).await?.is_some() {
            return Err(AppError::Conflict(
                "a menu is already published for that date and slot",
            ));
        }
        let menu = Menu {
            id: MenuId::generate(),
            date,
            slot,
            capacity,
            created_by: created_by.clone(),
            created_at: Timestamp::now(),
        };
        let created: Option<Menu> = db.create(menu.id.record()).content(menu).await?;
        created.ok_or_else(|| AppError::Internal("failed to publish the menu".into()))
    }

    pub async fn read(id: &MenuId, db: &Database) -> Result<Option<Menu>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The menu for one day and slot, if any — the uniqueness check.
    pub async fn find(
        date: &MenuDate,
        slot: &MenuSlot,
        db: &Database,
    ) -> Result<Option<Menu>, AppError> {
        let mut result = db
            .query("SELECT * FROM menu WHERE date = $date AND slot = $slot LIMIT 1")
            .bind(("date", date.as_str().to_string()))
            .bind(("slot", slot.as_str().to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Menu>>(0)?.into_iter().next())
    }

    /// Menus, newest day first. `from`/`to` are inclusive `YYYY-MM-DD` bounds;
    /// either may be omitted. The comparison is lexical, which is chronological
    /// for this format.
    pub async fn list(
        from: Option<&MenuDate>,
        to: Option<&MenuDate>,
        db: &Database,
    ) -> Result<Vec<Menu>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM menu \
                 WHERE ($from = NONE OR date >= $from) AND ($to = NONE OR date <= $to) \
                 ORDER BY date DESC, slot ASC, id DESC",
            )
            .bind(("from", from.map(|date| date.as_str().to_string())))
            .bind(("to", to.map(|date| date.as_str().to_string())))
            .await?
            .check()?;
        Ok(result.take::<Vec<Menu>>(0)?)
    }

    /// Only `capacity` is writable: `date` and `slot` are `READONLY` columns,
    /// because moving a published menu to another day is a different menu.
    /// `None` keeps the stored cap, `Some(None)` clears it back to uncapped.
    pub async fn update(
        self,
        capacity: Option<Option<i64>>,
        db: &Database,
    ) -> Result<Menu, AppError> {
        FieldUpdate::new(self.id.record())
            .set("capacity", capacity)
            .run::<Menu>(db)
            .await
    }

    /// Whether any menu was ever published for this slot name — the guard
    /// behind removing a slot from `meal_slots`, mirroring
    /// [`ExamResult::any_for_kind`](crate::domain::exam_result::ExamResult::any_for_kind).
    /// Menus snapshot the slot as text, so this is a plain string match.
    /// Callers hold [`MENU_LOCK`] so a publish cannot slip in behind the check.
    pub async fn any_for_slot(slot: &str, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM menu WHERE slot = $slot LIMIT 1")
            .bind(("slot", slot.to_string()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    /// Delete the menu and the dishes on it — a dish has no meaning without
    /// its menu, and the `menu` link is `READONLY`, so it cannot be re-homed.
    pub async fn delete(self, db: &Database) -> Result<Menu, AppError> {
        MenuDish::delete_for_menu(&self.id, db).await?;
        let deleted: Option<Menu> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_accepts_only_padded_calendar_days() {
        assert!(MenuDate::try_new("2026-07-26").is_ok());
        assert!(MenuDate::try_new("2026-7-26").is_err());
        assert!(MenuDate::try_new("2026-13-01").is_err());
        assert!(MenuDate::try_new("2026-00-01").is_err());
        assert!(MenuDate::try_new("2026-01-32").is_err());
        assert!(MenuDate::try_new("26-01-02").is_err());
        assert!(MenuDate::try_new("").is_err());
        // Sorting the text must sort the calendar — the range filter relies on it.
        assert!(
            MenuDate::try_new("2026-01-09").unwrap() < MenuDate::try_new("2026-01-10").unwrap()
        );
        assert!(
            MenuDate::try_new("2026-09-01").unwrap() < MenuDate::try_new("2026-10-01").unwrap()
        );
    }

    #[test]
    fn slot_must_be_one_the_school_serves() {
        let allowed = vec![MealSlotDef::try_new("lunch").unwrap()];
        assert!(MenuSlot::try_new("lunch", &allowed).is_ok());
        assert!(MenuSlot::try_new("brunch", &allowed).is_err());
        assert!(MenuSlot::try_new("lunch", &[]).is_err());
    }

    #[test]
    fn capacity_is_bounded_but_optional() {
        assert!(validate_capacity(None).is_ok());
        assert!(validate_capacity(Some(0)).is_ok());
        assert!(validate_capacity(Some(MAX_MENU_CAPACITY)).is_ok());
        assert!(validate_capacity(Some(-1)).is_err());
        assert!(validate_capacity(Some(MAX_MENU_CAPACITY + 1)).is_err());
    }
}
