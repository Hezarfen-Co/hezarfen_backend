//! A student's seat on a published [`Menu`]. One row per (menu, student),
//!
//! keyed by the natural composite primary key — the same pair always maps to
//! the same row, so booking is a single atomic upsert with no find-then-insert
//! race and one-row-per-pair by construction.
//!
//! This module is the pure row shape: the id and status, the booking row, and
//! the meal deadline policy — [`MealCutoff`] with the two checks that enforce
//! it, [`check_day_not_past`] and [`check_cutoff`]. The workflows that take
//! and free a seat live in [`crate::service::meal_booking`], and the
//! transactions that move the seat, its row and its money together in
//! [`crate::db::meal_booking`].
//!
//! Things that are deliberate:
//!
//! - **A cancel is a status transition, not a delete.** The row stays, flipped
//!   to `cancelled` with a `cancelled_at` stamp, so the freed seat is still
//!   auditable against the money it moved. Only `booked` rows count against
//!   the menu's capacity, which is what makes the seat genuinely free again —
//!   and re-booking is the same upsert flipping it back.
//! - **Capacity is the menu's `seats_booked` counter**, claimed by a
//!   conditional single-row write ([`cap`](crate::db::cap)). Counting the rows instead is
//!   write-skew — a cross-row count checked outside the insert can always be
//!   overtaken by a racing insert — and a process-wide lock around that count
//!   is released around the very round trip the racing insert lands in.
//!   Cancelling gives the seat back in the *same transaction* as the flip.
//! - **The price is claimed, not just read.** The seat is taken at the menu
//!   revision the price was read at, in the same transaction as the row
//!   ([`claim_and_place`](crate::db::meal_booking::claim_and_place)), so a dish re-priced mid-booking loses
//!   the claim and the booking re-reads both. A seat can only ever be billed a
//!   price its menu genuinely carried.
//! - **The seat and its money move together.** Booking charges and cancelling
//!   reverses in the service workflow ([`crate::service::meal_booking`]), not
//!   in the web layer — a seam between the two let concurrent `POST`s bill one
//!   seat twice — and *both* ledger lines are written by the very transaction
//!   that moves the seat ([`crate::db::meal_booking`]), because money that
//!   trails the seat by one write can be overtaken by the next attempt and
//!   stranded forever: every ledger id carries the attempt it belongs to.

use sqlx::Type;

use crate::domain::meal_ledger::LedgerAmount;
use crate::domain::menu::{MenuDate, MenuId, MenuSlot};
use crate::domain::settings::{MealSlotDef, Settings};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The (menu, student) pair — the table's natural composite primary key.
/// UUID strings carry only `-`, so `_` is an unambiguous joiner — and the
/// student half is last, so the menu key (a `{date}_{slot}` pair, itself
/// underscored) reads back whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MealBookingId {
    menu: MenuId,
    student: UserId,
}

impl MealBookingId {
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

    /// Who the seat is for, read straight off the pair.
    ///
    /// This is what lets a cancel decide *whose* seat it is asked to free
    /// before reading the row — the id is fully derivable from a menu and a
    /// user id, both readable by a teacher, so a 403-or-404 answered off the
    /// row is an existence oracle for the manager-only booking list.
    pub fn student(&self) -> UserId {
        self.student
    }

    pub fn menu(&self) -> &MenuId {
        &self.menu
    }
}

/// Where a booking stands. Stored as the bare lowercase TEXT value the
/// `status` column carries, exactly like
/// [`AppointmentStatus`](crate::domain::appointment::AppointmentStatus).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MealBooking {
    pub(crate) menu: MenuId,
    pub(crate) student: UserId,
    pub(crate) booked_by: UserId,
    pub(crate) status: MealBookingStatus,
    /// How many times this seat has been taken: 1 on the first booking, +1 on
    /// every revival after a cancel. It is the *attempt* half of the ledger's
    /// idempotence key, so a re-book bills afresh while a repeat `POST` of a
    /// seat already held bills nothing.
    pub(crate) attempt: i64,
    /// What the menu cost when the current attempt took the seat; `NULL` means
    /// it was free *then*. Recorded rather than inferred: without it, "the menu
    /// was free" is indistinguishable from "not billed yet", and a dish added
    /// afterwards bills a seat that was free when taken.
    pub(crate) price_minor: Option<LedgerAmount>,
    pub(crate) cancelled_at: Option<Timestamp>,
    pub(crate) created_at: Timestamp,
}

impl MealBooking {
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

    /// The id of the seat this row holds — the pair that is the row's primary
    /// key.
    pub fn id(&self) -> MealBookingId {
        MealBookingId::composite(&self.menu, &self.student)
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
    pub(crate) minutes: Option<i64>,
    pub(crate) slots: Vec<MealSlotDef>,
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
/// too — a UTC+3 school enters 09:00 for a noon lunch.
///
/// `None` for a date no day can be parsed out of, which is the only thing
/// [`check_cutoff`] uses the midnight case for: a slot with no serving minute
/// gets **no deadline at all** there, never a deadline counted from midnight.
fn served_at(date: &MenuDate, serving_minute: Option<i64>) -> Option<Timestamp> {
    let day = chrono::NaiveDate::parse_from_str(date.as_str(), "%Y-%m-%d").ok()?;
    Some(Timestamp::from_millis(
        day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis()
            + serving_minute.unwrap_or(0).saturating_mul(60_000),
    ))
}

/// A meal whose **calendar day is over** takes no more seats, whatever the
/// school configured. The booking is what charges (attendance never moves
/// money), so a seat taken on a day already served mints a real ledger line for
/// food nobody can be served — the one thing no route may do.
///
/// Deliberately **not** part of [`check_cutoff`], and deliberately not
/// configurable:
///
/// - The cutoff is a *policy* about the serving hour, and it is unenforced by
///   design when the school sets no `meal_cancel_cutoff_minutes` or the slot no
///   `serving_minute` (all three shipped slots carry none) — see the note there
///   for why the canteen must not go offline over an unset hour. This is not a
///   deadline before the meal at all: it is the day itself having passed, which
///   no configuration can make untrue. **Today's menu is untouched by it** —
///   with or without a serving hour, at any time of day.
/// - It binds `book` only. Cancelling stays open on a past day, cutoff bypass
///   or not: a cancel *reverses* a charge, and money already taken has to stay
///   reachable (that is the same reason manager+ bypasses the cutoff in
///   [`cancel_booking`](crate::web::meals)).
pub(crate) fn check_day_not_past(date: &MenuDate) -> Result<(), AppError> {
    // An unparsable day is left to `check_cutoff`, which fails it closed
    // whenever a cutoff is configured: guessing "past" from text no calendar
    // can place would shut the canteen for a school that set no deadline.
    let Some(over) = date.day_end() else {
        return Ok(());
    };
    if Timestamp::now().as_millis() >= over.as_millis() {
        return Err(AppError::Conflict(
            "that meal's day has passed, so its menu takes no more bookings",
        ));
    }
    Ok(())
}

/// Booking and cancelling both close `cutoff.minutes` before the meal is
/// served. `None` = the school set no cutoff, so neither ever closes.
///
/// **A slot with no `serving_minute` closes nothing either.** There is no
/// instant to count the deadline back from, and counting from midnight UTC —
/// as this did — put the deadline of every same-day menu in the past the
/// moment a school set `meal_cancel_cutoff_minutes`: today's lunch could not be
/// booked (`409`), and the seats already held could not be cancelled by the
/// students and parents holding them, only by a manager. All three shipped
/// slots carry no serving minute, so setting the one cutoff knob took the
/// canteen offline — which is precisely what the midnight fallback was chosen
/// to avoid. An unset serving hour is now an unenforced cutoff: the school sets
/// the hour and the deadline starts binding, on menus already published too
/// (the slot list is read live).
///
/// A date no serving instant can be computed from **fails closed**. A menu on
/// an impossible day (a 31st of February — refused by [`MenuDate`]) otherwise
/// skipped the deadline entirely, silently and forever, however large the
/// school set it: the one menu with no cutoff at all would be the one nobody
/// meant to publish. Refusing is the only answer that keeps "the deadline
/// binds every menu" true; the menu has to be republished on a real day to
/// become bookable again.
pub(crate) fn check_cutoff(
    date: &MenuDate,
    slot: &MenuSlot,
    cutoff: &MealCutoff,
) -> Result<(), AppError> {
    // A school with no cutoff configured closes nothing anyway, so an
    // impossible date is not refused there either — that would take the
    // canteen offline for a deadline the school never set.
    let Some(minutes) = cutoff.minutes else {
        return Ok(());
    };
    let serving_minute = cutoff.serving_minute(slot);
    // The impossible day is refused first, serving hour or not: it is a data
    // defect, and "the deadline binds every menu" has to stay true for the one
    // menu nobody meant to publish.
    let Some(serving) = served_at(date, serving_minute) else {
        return Err(AppError::Conflict(
            "the menu's date is not a real calendar day, so its cutoff cannot be worked out",
        ));
    };
    // No serving hour, no instant to count back from, so nothing closes.
    if serving_minute.is_none() {
        return Ok(());
    }
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
    use super::*;

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

    /// A slot with no serving hour has no instant to count a deadline back
    /// from, so nothing on it ever closes. It used to count from midnight UTC
    /// of the menu's day, which shut every same-day menu the moment a school
    /// set the cutoff knob — and all three shipped slots carry no hour.
    #[test]
    fn a_slot_without_a_serving_hour_has_no_deadline() {
        let past = MenuDate::try_new("2000-01-01").unwrap();
        assert!(check_cutoff(&past, &lunch(), &cutoff(Some(60), None)).is_ok());
        // The same menu, once the school sets the hour.
        assert!(check_cutoff(&past, &lunch(), &cutoff(Some(60), Some(720))).is_err());
    }

    /// Through the crate's one clock ([`Timestamp::today_utc`]), never
    /// `chrono::Utc::now` — the guard under test compares against that same
    /// clock, and a test reading a second one is how a timezone mix gets back
    /// in (`clippy.toml` denies it, tests included).
    fn today() -> String {
        Timestamp::today_utc().format("%Y-%m-%d").to_string()
    }

    /// The day being over is not the cutoff. It binds with no
    /// `meal_cancel_cutoff_minutes` set and no `serving_minute` on the slot —
    /// the shipped defaults, under which the cutoff deliberately closes nothing
    /// — and it never binds today, at any hour.
    #[test]
    fn the_past_day_guard_is_not_the_serving_hour_cutoff() {
        let yesterday = Timestamp::today_utc()
            .pred_opt()
            .expect("there is a day before today")
            .format("%Y-%m-%d")
            .to_string();
        assert!(check_day_not_past(&MenuDate::try_new(&yesterday).unwrap()).is_err());
        assert!(check_day_not_past(&MenuDate::try_new(&today()).unwrap()).is_ok());
        assert!(check_day_not_past(&MenuDate::try_new("2999-01-01").unwrap()).is_ok());
        // Today's menu is open all day *and* carries no deadline at all under
        // the shipped defaults — the two answers stay separate.
        assert!(
            check_cutoff(
                &MenuDate::try_new(&today()).unwrap(),
                &lunch(),
                &cutoff(Some(60), None)
            )
            .is_ok()
        );
    }

    /// The instant the deadline counts back from: midnight UTC of the day plus
    /// the slot's serving minute. The `None` case still resolves to midnight,
    /// but only as the date's validity probe — `check_cutoff` never counts a
    /// deadline from it, which the case below pins.
    #[test]
    fn serving_instant_offsets_midnight_utc() {
        let day = MenuDate::try_new("1970-01-02").unwrap();
        assert_eq!(served_at(&day, None).unwrap().as_millis(), 86_400_000);
        assert_eq!(
            served_at(&day, Some(12 * 60)).unwrap().as_millis(),
            86_400_000 + 12 * 60 * 60_000
        );
        // A slot the school no longer lists (renamed or retired after the
        // menu snapshotted its name) resolves to no serving time, which
        // `check_cutoff` reads as no deadline at all.
        assert_eq!(
            cutoff(Some(60), Some(720)).serving_minute(&lunch()),
            Some(720)
        );
        assert_eq!(MealCutoff::default().serving_minute(&lunch()), None);
    }

    #[test]
    fn status_as_str_is_the_storage_form() {
        assert_eq!(MealBookingStatus::Booked.as_str(), "booked");
        assert_eq!(MealBookingStatus::Cancelled.as_str(), "cancelled");
    }

    /// The `status` column is `TEXT` with a CHECK on these exact words; the
    /// sqlx encoding must never drift from `as_str`, or the `status =
    /// 'booked'` capacity count silently matches nothing.
    #[test]
    fn sqlx_encodes_the_storage_form() {
        let mut buf = sqlx::postgres::PgArgumentBuffer::default();
        for status in [MealBookingStatus::Booked, MealBookingStatus::Cancelled] {
            buf.clear();
            sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&status, &mut buf);
            assert_eq!(std::str::from_utf8(&buf).unwrap(), status.as_str());
        }
    }

    /// The composite key round-trips: the menu half (itself underscored) reads
    /// back whole, and a key that parses as no pair reads as the nil pair that
    /// matches no row.
    #[test]
    fn the_booking_key_round_trips() {
        let menu = MenuId::for_slot(
            &MenuDate::try_new("2026-09-14").unwrap(),
            &MenuSlot::try_new(
                "öğle yemeği",
                &[MealSlotDef::try_new("öğle yemeği", None).unwrap()],
            )
            .unwrap(),
        );
        let student = UserId::from_key("018f1a00-0000-7000-8000-000000000001");
        let id = MealBookingId::composite(&menu, &student);
        assert_eq!(MealBookingId::from_key(&id.key()), id);
        assert_eq!(
            MealBookingId::from_key("no-pair-here").student(),
            UserId::from_key("")
        );
        assert_eq!(
            MealBookingId::from_key("no-pair-here").menu.key(),
            MenuId::from_key("").key()
        );
    }
}
