//! A student's seat on a published [`Menu`]. One row per (menu, student),
//! keyed by a deterministic composite id — the same pair always maps to the
//! same record, so booking is a single atomic UPSERT with no find-then-insert
//! race and one-row-per-pair by construction.
//!
//! Two things are deliberate:
//!
//! - **A cancel is a status transition, not a delete.** The row stays, flipped
//!   to `cancelled` with a `cancelled_at` stamp, so the freed seat is still
//!   auditable against the money it moved. Only `booked` rows count against
//!   the menu's capacity, which is what makes the seat genuinely free again —
//!   and re-booking is the same UPSERT flipping it back.
//! - **The capacity check runs under [`MENU_LOCK`]**, the lock that already
//!   serializes everything counting rows against one menu. Count-then-write is
//!   write-skew: SurrealDB's transactions do not conflict-check a cross-record
//!   count against a concurrent insert, so an unlocked check over-admits.
//! - **The seat and its money move together, inside that lock.** Booking
//!   charges and cancelling reverses right here, not in the web layer after the
//!   lock was dropped — a seam between the two let concurrent `POST`s bill one
//!   seat twice. The lock stays a *leaf*: nothing below it takes another.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::MEAL_BOOKING_TABLE;
use crate::database::Database;
use crate::domain::meal_ledger::{LedgerAmount, MealLedger};
use crate::domain::menu::{MENU_LOCK, Menu, MenuDate, MenuId, MenuSlot};
use crate::domain::settings::{MealSlotDef, Settings};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MealBookingId(RecordId);

impl MealBookingId {
    /// A deterministic id for the (menu, student) pair — same trick as
    /// `EnrollmentId`. ULID keys are alphanumeric, so `_` is an unambiguous
    /// joiner.
    pub fn composite(menu: &MenuId, student: &UserId) -> Self {
        Self(RecordId::new(
            MEAL_BOOKING_TABLE,
            format!("{}_{}", menu.key(), student.key()),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(MEAL_BOOKING_TABLE, key))
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
/// lowercase string the `status` column types as, exactly like
/// [`AppointmentStatus`](crate::domain::appointment::AppointmentStatus).
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum MealBookingStatus {
    Booked,
    Cancelled,
}

impl MealBookingStatus {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            MealBookingStatus::Booked => "booked",
            MealBookingStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct MealBooking {
    id: MealBookingId,
    menu: MenuId,
    student: UserId,
    booked_by: UserId,
    status: MealBookingStatus,
    /// How many times this seat has been taken: 1 on the first booking, +1 on
    /// every revival after a cancel. It is the *attempt* half of the ledger's
    /// idempotence key, so a re-book bills afresh while a repeat `POST` of a
    /// seat already held bills nothing.
    attempt: i64,
    /// What the menu cost when the current attempt took the seat; `None` means
    /// it was free *then*. Recorded rather than inferred: without it, "the menu
    /// was free" is indistinguishable from "not billed yet", and a dish added
    /// afterwards bills a seat that was free when taken.
    price_minor: Option<LedgerAmount>,
    cancelled_at: Option<Timestamp>,
    created_at: Timestamp,
}

impl MealBooking {
    pub fn get_id(&self) -> &MealBookingId {
        &self.id
    }

    pub fn get_menu(&self) -> &MenuId {
        &self.menu
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_booked_by(&self) -> &UserId {
        &self.booked_by
    }

    pub fn get_status(&self) -> MealBookingStatus {
        self.status
    }

    pub fn get_attempt(&self) -> i64 {
        self.attempt
    }

    pub fn get_price_minor(&self) -> Option<LedgerAmount> {
        self.price_minor
    }

    pub fn get_cancelled_at(&self) -> Option<Timestamp> {
        self.cancelled_at
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Book (idempotently) `student` onto `menu` **and bill the seat**. Already
    /// booked is returned as-is; a cancelled row is revived by the same UPSERT,
    /// as a fresh attempt at the current `price`. When the menu carries a
    /// capacity, a full one refuses new seats (409) — the whole
    /// check-then-write runs under [`MENU_LOCK`], and the cap is re-derived
    /// from a fresh menu read inside it, since neither the transaction model
    /// nor a caller-supplied menu survives a concurrent capacity PATCH.
    ///
    /// The price is snapshotted **inside that same lock**, not handed in by the
    /// web layer: dish writes take the lock too, so the sum frozen onto the row
    /// is the menu as it stood the instant the seat was taken. Snapshotted
    /// outside it, a dish landing in the gap was stored as `None` = "was free
    /// then" — and no repeat `POST` ever heals that, since a held seat is
    /// returned as-is.
    ///
    /// The charge is appended inside the lock too, keyed by (seat, attempt):
    /// eight concurrent `POST`s of one seat hold one seat and write one charge
    /// line. The price is `None` when the menu is free, and that `None` is
    /// stored, so a dish added later never bills a seat retroactively.
    ///
    /// `cutoff` is the school's one meal deadline (see [`MealCutoff`]): once
    /// serving time is that close, the seat is fixed either way.
    pub async fn book(
        menu: &MenuId,
        student: &UserId,
        booked_by: &UserId,
        cutoff: &MealCutoff,
        db: &Database,
    ) -> Result<MealBooking, AppError> {
        let _guard = MENU_LOCK.lock().await;
        let fresh = Menu::read(menu, db).await?.ok_or(AppError::NotFound)?;
        check_cutoff(fresh.get_date(), fresh.get_slot(), cutoff)?;
        // Before the seat: an unchargeable menu (dishes summing past the cap)
        // must refuse the booking outright, never leave a booked-but-unbilled
        // row behind.
        let price = MealLedger::price_snapshot(menu, db).await?;
        let id = MealBookingId::composite(menu, student);
        let existing: Option<MealBooking> = db.select(id.record()).await?;
        let booking = match existing {
            // The seat is already held: same attempt, same price it was taken
            // at. Today's menu price bills nobody who booked yesterday's.
            Some(held) if held.status == MealBookingStatus::Booked => held,
            existing => {
                if let Some(capacity) = fresh.get_capacity()
                    && Self::list_live_for_menu(menu, db).await?.len() as i64 >= capacity
                {
                    return Err(AppError::Conflict("the menu is fully booked"));
                }
                let booking = MealBooking {
                    id,
                    menu: menu.clone(),
                    student: student.clone(),
                    // `booked_by` and `created_at` are READONLY columns: a
                    // revived row keeps whoever opened the seat, and when.
                    booked_by: existing
                        .as_ref()
                        .map_or_else(|| booked_by.clone(), |row| row.booked_by.clone()),
                    status: MealBookingStatus::Booked,
                    attempt: existing.as_ref().map_or(1, |row| row.attempt + 1),
                    price_minor: price,
                    cancelled_at: None,
                    created_at: existing
                        .as_ref()
                        .map_or_else(Timestamp::now, |row| row.created_at),
                };
                let saved: Option<MealBooking> =
                    db.upsert(booking.id.record()).content(booking).await?;
                saved.ok_or_else(|| AppError::Internal("failed to book the meal".into()))?
            }
        };
        // Writes nothing when this attempt is already billed, so the repeat
        // POST above costs nothing and a charge that failed once self-heals.
        MealLedger::charge_booking(&booking, booked_by, db).await?;
        Ok(booking)
    }

    /// Free the seat: flip to `cancelled`, stamp the moment, **and give the
    /// money back**. Refused (409) only once the cutoff has passed.
    ///
    /// Under [`MENU_LOCK`] like [`Self::book`], and the row is re-read inside
    /// it: the caller's copy may predate a concurrent re-book, and cancelling a
    /// stale attempt would strand that attempt's charge forever. The reversal
    /// is keyed to the attempt, so a retried cancel refunds exactly once.
    ///
    /// **Idempotent, and that is what makes the refund recoverable.** The flip
    /// and the reversal are two writes; a handler future dropped between them
    /// (the client closed the tab, a proxy timed out) or a `create` that errors
    /// leaves the seat free and the charge standing. Refusing an
    /// already-cancelled row — as this used to — made that state permanent:
    /// no route on the API could ever append the missing reversal. So a
    /// cancelled row replays the reversal instead, which either heals the gap
    /// or writes nothing at all. The cutoff is not re-checked on that path: the
    /// seat is already given back, and refusing to finish the refund because
    /// the meal has since closed would strand the money exactly as before.
    pub async fn cancel(
        self,
        cutoff: &MealCutoff,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<MealBooking, AppError> {
        let _guard = MENU_LOCK.lock().await;
        let fresh = Self::read(&self.id, db).await?.ok_or(AppError::NotFound)?;
        if fresh.status == MealBookingStatus::Cancelled {
            MealLedger::reverse_booking(&fresh, recorded_by, db).await?;
            return Ok(fresh);
        }
        let menu = Menu::read(&fresh.menu, db)
            .await?
            .ok_or(AppError::NotFound)?;
        check_cutoff(menu.get_date(), menu.get_slot(), cutoff)?;
        let cancelled = MealBooking {
            status: MealBookingStatus::Cancelled,
            cancelled_at: Some(Timestamp::now()),
            ..fresh
        };
        let saved: Option<MealBooking> =
            db.upsert(cancelled.id.record()).content(cancelled).await?;
        let saved =
            saved.ok_or_else(|| AppError::Internal("failed to cancel the booking".into()))?;
        MealLedger::reverse_booking(&saved, recorded_by, db).await?;
        Ok(saved)
    }

    pub async fn read(id: &MealBookingId, db: &Database) -> Result<Option<MealBooking>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every booking on a menu, cancelled ones included — the kitchen's list.
    pub async fn list_for_menu(menu: &MenuId, db: &Database) -> Result<Vec<MealBooking>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM meal_booking WHERE menu = $menu \
                 ORDER BY created_at DESC, id DESC",
            )
            .bind(("menu", menu.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<MealBooking>>(0)?)
    }

    /// The seats actually held — the capacity count.
    pub async fn list_live_for_menu(
        menu: &MenuId,
        db: &Database,
    ) -> Result<Vec<MealBooking>, AppError> {
        let mut result = db
            .query("SELECT * FROM meal_booking WHERE menu = $menu AND status = 'booked'")
            .bind(("menu", menu.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<MealBooking>>(0)?)
    }

    /// True iff a seat on the menu is still held — the menu delete guard.
    /// Callers hold [`MENU_LOCK`] so a booking cannot slip in behind it.
    pub async fn any_live_for_menu(menu: &MenuId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE id FROM meal_booking \
                 WHERE menu = $menu AND status = 'booked' LIMIT 1",
            )
            .bind(("menu", menu.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    /// Every seat held for one of `students`, newest first — a caller's own
    /// list is themselves plus whoever they hold a *live* parent link to.
    ///
    /// Deliberately **not** `booked_by = $usr`: who placed a booking is history
    /// written onto a READONLY column, and history is not a read grant. A
    /// parent whose link was revoked (by an unlink or by the student-side role
    /// sweep) would otherwise keep a live view of the child's seat, watching
    /// cancellations made long after the link died — exactly what
    /// [`ensure_can_observe`](crate::web::ensure_can_observe) refuses. The
    /// caller re-derives the list from the links on every read, so the view
    /// dies with the link.
    pub async fn list_for_students(
        students: &[UserId],
        db: &Database,
    ) -> Result<Vec<MealBooking>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM meal_booking WHERE student IN $students \
                 ORDER BY created_at DESC, id DESC",
            )
            .bind((
                "students",
                students.iter().map(UserId::record).collect::<Vec<_>>(),
            ))
            .await?
            .check()?;
        Ok(result.take::<Vec<MealBooking>>(0)?)
    }
}

/// The school's meal deadline policy, resolved once per request: how many
/// minutes ahead booking and cancelling close, plus the slot list the serving
/// times live on. Built from [`Settings`] by the web layer and handed down, so
/// the domain never reaches for the singleton mid-lock.
///
/// The slots are read **live**, not snapshotted onto the menu, and that is
/// deliberate: `meal_cancel_cutoff_minutes` is already read live, so freezing
/// the other half of the same deadline would make one policy edit apply and
/// its twin not. A kitchen that moves lunch an hour later wants today's menus
/// to move with it; the menu still snapshots the slot *name*, which is what
/// keeps a retired slot's history readable.
#[derive(Debug, Clone, Default)]
pub struct MealCutoff {
    minutes: Option<i64>,
    slots: Vec<MealSlotDef>,
}

impl MealCutoff {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            minutes: settings.get_meal_cancel_cutoff_minutes(),
            slots: settings.get_meal_slots(),
        }
    }

    /// Minutes past midnight UTC at which `slot` is served, or `None` when the
    /// school set none (or dropped the slot after a menu snapshotted its name).
    fn serving_minute(&self, slot: &MenuSlot) -> Option<i64> {
        self.slots
            .iter()
            .find(|def| def.get_name() == slot.as_str())
            .and_then(MealSlotDef::get_serving_minute)
    }
}

/// The instant the meal is served: midnight UTC of `date` plus the slot's
/// `serving_minute`. `date` is text with no timezone (see [`MenuDate`]) and the
/// backend deliberately stores no school timezone, so the serving time is UTC
/// too — a UTC+3 school enters 09:00 for a noon lunch. A slot with no serving
/// time falls back to midnight UTC, exactly what every booking used before the
/// field existed; refusing the booking instead would take the canteen offline
/// on an upgrade.
fn served_at(date: &MenuDate, serving_minute: Option<i64>) -> Option<Timestamp> {
    let day = chrono::NaiveDate::parse_from_str(date.as_str(), "%Y-%m-%d").ok()?;
    Some(Timestamp::from_millis(
        day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis()
            + serving_minute.unwrap_or(0).saturating_mul(60_000),
    ))
}

/// Booking and cancelling both close `cutoff.minutes` before the meal is
/// served. `None` = the school set no cutoff, so neither ever closes.
fn check_cutoff(date: &MenuDate, slot: &MenuSlot, cutoff: &MealCutoff) -> Result<(), AppError> {
    let serving = served_at(date, cutoff.serving_minute(slot));
    let (Some(minutes), Some(serving)) = (cutoff.minutes, serving) else {
        return Ok(());
    };
    let deadline = serving
        .as_millis()
        .saturating_sub(minutes.saturating_mul(60_000));
    if Timestamp::now().as_millis() >= deadline {
        return Err(AppError::Conflict("the menu's booking cutoff has passed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use surrealdb::types::SurrealValue as _;
    use surrealdb::types::Value;

    use super::*;

    /// The `status` column is `TYPE string`: an object-wrapped enum would be
    /// rejected on write, and the `status = 'booked'` capacity count would
    /// silently match nothing.
    #[test]
    fn status_stores_as_a_bare_string() {
        for status in [MealBookingStatus::Booked, MealBookingStatus::Cancelled] {
            let value = status.into_value();
            assert_eq!(value, Value::String(status.as_str().to_string()));
            assert_eq!(MealBookingStatus::from_value(value).unwrap(), status);
        }
    }

    fn cutoff(minutes: Option<i64>, serving_minute: Option<i64>) -> MealCutoff {
        MealCutoff {
            minutes,
            slots: vec![MealSlotDef::try_new("lunch", serving_minute).unwrap()],
        }
    }

    fn lunch() -> MenuSlot {
        MenuSlot::try_new("lunch", &[MealSlotDef::try_new("lunch", None).unwrap()]).unwrap()
    }

    #[test]
    fn cutoff_closes_only_within_the_window() {
        let far = MenuDate::try_new("2999-01-01").unwrap();
        let past = MenuDate::try_new("2000-01-01").unwrap();
        // No cutoff configured: nothing ever closes, not even a past day.
        assert!(check_cutoff(&past, &lunch(), &cutoff(None, Some(720))).is_ok());
        // A day far ahead is open; one long gone is shut.
        assert!(check_cutoff(&far, &lunch(), &cutoff(Some(60), Some(720))).is_ok());
        assert!(check_cutoff(&past, &lunch(), &cutoff(Some(60), Some(720))).is_err());
    }

    /// The instant the deadline counts back from: midnight UTC of the day plus
    /// the slot's serving minute, and midnight itself when the slot has none.
    #[test]
    fn serving_instant_offsets_midnight_utc() {
        let day = MenuDate::try_new("1970-01-02").unwrap();
        assert_eq!(served_at(&day, None).unwrap().as_millis(), 86_400_000);
        assert_eq!(
            served_at(&day, Some(12 * 60)).unwrap().as_millis(),
            86_400_000 + 12 * 60 * 60_000
        );
        // A slot the school no longer lists (renamed or retired after the
        // menu snapshotted its name) resolves to no serving time, i.e. the
        // midnight fallback rather than a refusal.
        assert_eq!(
            cutoff(Some(60), Some(720)).serving_minute(&lunch()),
            Some(720)
        );
        assert_eq!(MealCutoff::default().serving_minute(&lunch()), None);
    }
}
