//! Meal-booking workflows: taking a seat and giving it back, with the seat's
//! money folded into the very decision that moves it. Both writes are single
//! transactions in [`crate::db::meal_booking`] — [`book`] prices the seat,
//! claims it at the revision it priced against
//! ([`meal_booking::claim_and_place`]) and bills it in that same
//! transaction; [`cancel`] re-reads, re-checks the deadline, frees the seat
//! and reverses the charge through [`meal_booking::release_seat`] and its
//! heal-up paths.
//!
//! **No process-wide lock guards this domain**, and deliberately: every
//! invariant booking depends on is a *single-record* conditional write (the
//! menu's `seats_booked` counter and the attempt-fenced row flip), which the
//! guarded writes themselves serialize — a cross-record lock would add
//! contention without deciding anything the transactions do not already.
//! What the
//! service layer owns here is the *sequence*: read the menu, refuse a past
//! day or a closed cutoff before anything is claimed, settle the attempt
//! number before the charge id exists, and map the claim's outcomes
//! (made / duplicate / full) onto answers on fresh reads.

use crate::constant::{CAP_WRITE_TRIES, MAX_MEAL_BOOKING_ATTEMPTS};
use crate::database::{Database, backoff};
use crate::db::cap;
use crate::db::meal_booking;
use crate::db::meal_ledger;
use crate::db::menu;
use crate::domain::meal_booking::{
    MealBooking, MealBookingId, MealBookingStatus, MealCutoff, check_cutoff, check_day_not_past,
};
use crate::domain::meal_ledger::MealLedger;
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::service::meal_ledger::{charge_booking, reverse_booking};

/// Book (idempotently) `student` onto `menu` **and bill the seat**. Already
/// booked is returned as-is, claiming nothing; a cancelled row is revived as
/// a fresh attempt at the current price. A full menu refuses the seat (409),
/// decided by the counter's own `WHERE` rather than by a count this process
/// took a moment ago.
///
/// The price is read from the menu and then **claimed with it**: the seat is
/// only taken while the menu still stands at the revision the price was read
/// at, so a dish added, re-priced or removed in that gap loses the claim and
/// the whole decision is made again on fresh figures. What is frozen onto
/// the row is therefore always a price the menu genuinely carried at the
/// instant the seat was taken — a guarantee a lock around the read could
/// not give, since the price is read a round trip before the claim.
///
/// The charge rides that same transaction, keyed by (seat, attempt): eight
/// concurrent `POST`s of one seat hold one seat and write one charge line.
/// The price is `None` when the menu is free, and that `None` is stored, so
/// a dish added later never bills a seat retroactively.
///
/// `cutoff` is the school's one meal deadline (see [`MealCutoff`]): once
/// serving time is that close, the seat is fixed either way.
pub async fn book(
    db: &Database,
    menu: &MenuId,
    student: &UserId,
    booked_by: &UserId,
    cutoff: &MealCutoff,
) -> Result<MealBooking, AppError> {
    let id = MealBookingId::composite(menu, student);
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let fresh = menu::read(db, menu).await?.ok_or(AppError::NotFound)?;
        check_day_not_past(fresh.get_date())?;
        check_cutoff(fresh.get_date(), fresh.get_slot(), cutoff)?;
        let existing: Option<MealBooking> = meal_booking::read(db, &id).await?;
        // The seat is already held: same attempt, same price it was taken
        // at. Today's menu price bills nobody who booked yesterday's. No
        // seat is claimed, so a double-click cannot count two.
        //
        // Answered *before* the menu is priced, and that ordering is the
        // rule: pricing first made a repeat POST of a seat already held 400
        // ("amount_minor must be between…") as soon as the menu's dishes
        // summed past the chargeable maximum — refusing to replay a seat
        // the student holds, over a price that seat was never going to be
        // billed.
        if let Some(held) = existing
            .clone()
            .filter(|row| row.status == MealBookingStatus::Booked)
        {
            charge_booking(db, &held, booked_by).await?;
            // Answered off a re-read, never off the row read a round trip
            // ago: a cancel committing in that gap has freed the seat,
            // given the money back and left the row `cancelled`, and
            // returning `held` then reports a seat — `"status": "booked"`,
            // `cancelled_at: null` — the store does not hold. The stored
            // state was right all along; only the answer lied. Gone or
            // cancelled, the decision is simply made again, exactly as
            // `Claimed::Duplicate` below does, so the caller ends up with
            // the seat they asked for rather than a `201` about a seat
            // somebody already released.
            match meal_booking::read(db, &id).await? {
                Some(live) if live.status == MealBookingStatus::Booked => return Ok(live),
                _ => continue,
            }
        }
        // The attempt is settled *here*, not by the revival's `attempt + 1`,
        // because the charge id is (seat, attempt) and the charge has to be
        // built before the transaction that writes it. `existing` is either
        // absent or cancelled — a booked row returned above.
        let attempt = existing.as_ref().map_or(1, |prior| prior.attempt + 1);
        // A seat retaken without end is a ledger without end: each cycle
        // appends a charge and its reversal, both permanent, and every
        // later balance read is answered over the lot. Refused *before* the
        // menu is priced, so the answer is the ceiling rather than whatever
        // the pricing happens to say. A row already past the ceiling — an
        // upgrade's leftovers — is untouched by this: cancelling consults
        // no cap, so its seat and its money stay reachable, and only one
        // more revival is refused.
        if attempt > MAX_MEAL_BOOKING_ATTEMPTS {
            return Err(AppError::Conflict(
                "this seat has been booked and cancelled its maximum number of times",
            ));
        }
        // Before the seat: an unchargeable menu (dishes summing past the
        // cap) must refuse the booking outright, never leave a
        // booked-but-unbilled row behind.
        let price = meal_ledger::price_snapshot(db, menu).await?;
        let fresh_row = MealBooking {
            menu: menu.clone(),
            student: *student,
            booked_by: *booked_by,
            status: MealBookingStatus::Booked,
            attempt,
            price_minor: price,
            cancelled_at: None,
            created_at: Timestamp::now(),
        };
        let charge = MealLedger::charge_for(&fresh_row, booked_by);
        match meal_booking::claim_and_place(
            db,
            menu,
            fresh.get_capacity().unwrap_or(cap::UNLIMITED),
            fresh.get_version(),
            &fresh_row,
            price,
            charge.as_ref(),
        )
        .await?
        {
            // Seat, row and charge committed together; nothing is owed
            // after this call, which is the whole point of folding them.
            cap::Claimed::Made(booking) => return Ok(booking),
            // Another `POST` of this very pair moved the row first, and this
            // caller never took a seat (the whole transaction rolled back).
            // The decision is simply made again rather than answered off a
            // row read here: the winner may have left it `cancelled` at an
            // attempt this call never saw, and the loop's own held-seat path
            // is what returns a live seat *and* replays its charge.
            cap::Claimed::Duplicate => continue,
            // Full, moved, or gone — one `WHERE` refused all three, so the
            // reason is re-read rather than guessed. A moved menu is not a
            // refusal: the price above is stale, so price and seat are
            // taken again together.
            cap::Claimed::Full => match menu::read(db, menu).await? {
                None => return Err(AppError::NotFound),
                Some(now) if now.get_version() != fresh.get_version() => continue,
                Some(_) => return Err(AppError::Conflict("the menu is fully booked")),
            },
        }
    }
    Err(AppError::Conflict(
        "the menu kept changing underneath this booking",
    ))
}

/// The row, for callers that only inspect it — the web layer's authorization
/// gates read through here.
pub async fn read(db: &Database, id: &MealBookingId) -> Result<Option<MealBooking>, AppError> {
    meal_booking::read(db, id).await
}

/// Every booking on a menu, cancelled ones included — the kitchen's list.
pub async fn list_for_menu(
    db: &Database,
    menu: &MenuId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealBooking>, i64), AppError> {
    meal_booking::list_for_menu(db, menu, limit, offset).await
}

/// Every seat held for one of `students`, newest first — a caller's own
/// list is themselves plus whoever they hold a *live* parent link to.
/// The links are re-derived by the caller on every read, so the view dies
/// with the link.
pub async fn list_for_students(
    db: &Database,
    students: &[UserId],
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealBooking>, i64), AppError> {
    meal_booking::list_for_students(db, students, limit, offset).await
}

/// Free the seat: flip to `cancelled`, stamp the moment, **and give the
/// money back**. Refused (409) only once the cutoff has passed.
///
/// The row is re-read here and the flip is conditional on it still being
/// `booked`: the caller's copy may predate a concurrent re-book, and
/// cancelling a stale attempt would strand that attempt's charge forever.
/// The reversal is keyed to the attempt, so a retried cancel refunds once.
///
/// **The refund is not a second write.** It is appended by the very
/// transaction that flips the row (see
/// [`meal_booking::release_seat`]), so a
/// handler future dropped mid-cancel, a proxy timing out or the store going
/// away cannot leave the seat free with the charge standing — the state a
/// re-book then made permanent, since every ledger id carries the attempt
/// the re-book has already moved past.
///
/// **Idempotent all the same**, which is what heals a seat flipped before
/// that was true: a cancelled row replays the reversal instead of being
/// refused, and that either completes an old missing line or writes nothing
/// at all. The cutoff is not re-checked on that path: the seat is already
/// given back, and refusing to finish the refund because the meal has since
/// closed would strand the money exactly as before.
pub async fn cancel(
    db: &Database,
    cancelled: MealBooking,
    cutoff: &MealCutoff,
    recorded_by: &UserId,
) -> Result<MealBooking, AppError> {
    let fresh = meal_booking::read(db, &cancelled.id())
        .await?
        .ok_or(AppError::NotFound)?;
    if fresh.status == MealBookingStatus::Cancelled {
        reverse_booking(db, &fresh, recorded_by).await?;
        return Ok(fresh);
    }
    let menu = menu::read(db, &fresh.menu)
        .await?
        .ok_or(AppError::NotFound)?;
    check_cutoff(menu.get_date(), menu.get_slot(), cutoff)?;
    // Lost the flip: the row is no longer the `booked` attempt this call
    // read — another cancel took the seat back first, or a booking took it
    // again. Its row is the truth, but only the attempt *this* call was
    // cancelling may be refunded off it (see [`refundable_after_lost_flip`]).
    let saved = match meal_booking::release_seat(db, &fresh, recorded_by).await? {
        Some(cancelled) => cancelled,
        None => {
            let live = meal_booking::read(db, &cancelled.id())
                .await?
                .ok_or(AppError::NotFound)?;
            if !refundable_after_lost_flip(&fresh, &live)? {
                return Ok(live);
            }
            live
        }
    };
    reverse_booking(db, &saved, recorded_by).await?;
    Ok(saved)
}

/// May a cancel that released *nothing* still refund what it read?
///
/// Only when the row it finds is the very attempt it was cancelling, still
/// cancelled. A cancel is two writes, and between them the seat can be
/// re-booked: the row then reads `booked` at attempt N+1 with a live charge
/// against it, and reversing that — as re-reading and refunding blindly did
/// — refunds a seat that is still held *and* still counted, and burns the
/// `(booking, attempt)` ledger id that attempt's own cancel needs, so it
/// could never be refunded afterwards. A row at another attempt is the same
/// story one step further on: whoever cancelled attempt N appended its
/// reversal, and this call has nothing left to heal.
///
/// A seat that is `booked` again is a **409**, not a `200`: the flip was
/// fenced on the attempt this call read, so the row can only be held by a
/// booking that landed inside this very cancel — one the API answered `201`
/// for and which no caller here has seen, let alone decided to free. `200`
/// with a `booked` row would report a cancellation that did not happen; the
/// client is told to look again instead, and a fresh `DELETE` then cancels
/// the attempt it really read. (A repeat cancel of an *unchanged* seat never
/// reaches here: [`cancel`] answers an already-cancelled row above.)
fn refundable_after_lost_flip(seen: &MealBooking, live: &MealBooking) -> Result<bool, AppError> {
    if live.status == MealBookingStatus::Booked {
        return Err(AppError::Conflict(
            "the seat was booked again while this cancellation ran",
        ));
    }
    Ok(live.attempt == seen.attempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::db::menu_dish;
    use crate::domain::meal_booking::MealBookingStatus;
    use crate::domain::meal_ledger::LedgerAmount;
    use crate::domain::menu::{MenuDate, MenuSlot};
    use crate::domain::menu_dish::{DishName, DishPrice, DishTags, MenuDish};
    use crate::domain::settings::MealSlotDef;

    /// A published menu on a fresh in-memory database, plus a student. Dated
    /// well ahead on purpose: a menu whose day has passed refuses every
    /// booking, so a date the calendar overtakes would fail this whole module.
    async fn menu(capacity: Option<i64>) -> (Database, MenuId, crate::database::TestDatabases) {
        menu_on("2099-09-14", capacity).await
    }

    // The lease rides with the pool: dropping it here would drop the database
    // out from under the test that is about to run.
    async fn menu_on(
        date: &str,
        capacity: Option<i64>,
    ) -> (Database, MenuId, crate::database::TestDatabases) {
        let (db, _leases) = init_test_db().await;
        // The menu's creator is a foreign key now: a real `app_user` row.
        let creator = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at, role) \
             VALUES ($1, $2, 0, 'teacher')",
        )
        .bind(creator.uuid())
        .bind(format!("menu-fixture-{}", &creator.key()[30..]))
        .execute(&db)
        .await
        .unwrap();
        let slots = vec![MealSlotDef::try_new("lunch", None).unwrap()];
        let menu = menu::create(
            &db,
            MenuDate::try_new(date).unwrap(),
            MenuSlot::try_new("lunch", &slots).unwrap(),
            capacity,
            &creator,
        )
        .await
        .unwrap();
        (db, menu.get_id().clone(), _leases)
    }

    /// The stored counter, absent reading as zero — the number the cap's
    /// `WHERE` actually compares, not one recomputed from the rows.
    async fn seats(menu: &MenuId, db: &Database) -> i64 {
        use sqlx::Row as _;

        sqlx::query("SELECT COALESCE(seats_booked, 0) FROM menu WHERE id = $1")
            .bind(menu.key())
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }

    async fn add_dish(menu: &MenuId, price: i64, db: &Database) -> MenuDish {
        // A cap far above every seat count in these tests: the dish must never
        // be the binding constraint here.
        menu_dish::create(
            db,
            menu,
            DishName::try_new("çorba").unwrap(),
            None,
            DishPrice::try_new(price).unwrap(),
            DishTags::try_new(&[], &[]).unwrap(),
            1_000,
        )
        .await
        .unwrap()
    }

    /// The counter is the cap's authority, so it has to track the seats exactly:
    /// a repeat booking must not count twice, a cancel must hand its seat back
    /// in the same breath as the flip, and a second cancel must not hand back a
    /// seat that was already returned (which would open the cap by one forever).
    #[tokio::test]
    async fn the_seat_counter_follows_the_seats_it_guards() {
        let (db, menu, _leases) = menu(Some(1)).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        let veli = crate::db::class_member::tests::fixture_user(&db, "veli").await;
        let open = MealCutoff::default();

        let booking = book(&db, &menu, &ali, &ali, &open).await.unwrap();
        assert_eq!(seats(&menu, &db).await, 1);
        // The same seat again is a no-op, not a second claim.
        book(&db, &menu, &ali, &ali, &open).await.unwrap();
        assert_eq!(seats(&menu, &db).await, 1);
        // …and the cap bites for anyone else while it is held.
        assert!(matches!(
            book(&db, &menu, &veli, &veli, &open).await,
            Err(AppError::Conflict(_))
        ));

        let cancelled = cancel(&db, booking, &open, &ali).await.unwrap();
        assert_eq!(cancelled.get_status(), MealBookingStatus::Cancelled);
        assert_eq!(seats(&menu, &db).await, 0);
        // Cancelling again is idempotent all the way down to the counter.
        cancel(&db, cancelled, &open, &ali).await.unwrap();
        assert_eq!(seats(&menu, &db).await, 0);

        // The freed seat is real, and taking it again counts once.
        let revived = book(&db, &menu, &veli, &veli, &open).await.unwrap();
        assert_eq!(revived.get_attempt(), 1);
        assert_eq!(seats(&menu, &db).await, 1);
    }

    /// A menu whose row is gone or whose seats are held refuses its own delete,
    /// in the `WHERE` rather than in a read the delete then trusts.
    #[tokio::test]
    async fn a_held_seat_refuses_the_menu_delete() {
        let (db, id, _leases) = menu(None).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        let booking = book(&db, &id, &ali, &ali, &MealCutoff::default())
            .await
            .unwrap();
        let row = menu::read(&db, &id).await.unwrap().unwrap();
        assert!(matches!(
            menu::delete(&db, row.clone()).await,
            Err(AppError::Conflict(_))
        ));
        cancel(&db, booking, &MealCutoff::default(), &ali)
            .await
            .unwrap();
        assert!(menu::delete(&db, row).await.is_ok());
    }

    /// A booking placed after a price edit is billed the new price, and one
    /// placed before keeps the old one — the seat's `price_minor` is history,
    /// never re-read from the menu.
    #[tokio::test]
    async fn a_seat_keeps_the_price_it_was_taken_at() {
        let (db, menu, _leases) = menu(None).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        let veli = crate::db::class_member::tests::fixture_user(&db, "veli").await;
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;

        let early = book(&db, &menu, &ali, &ali, &open).await.unwrap();
        add_dish(&menu, 500, &db).await;
        let late = book(&db, &menu, &veli, &veli, &open).await.unwrap();

        assert_eq!(
            early.get_price_minor().map(LedgerAmount::as_minor),
            Some(1_000)
        );
        assert_eq!(
            late.get_price_minor().map(LedgerAmount::as_minor),
            Some(1_500)
        );
        // The seat taken before the second dish is not re-billed for it.
        let held = book(&db, &menu, &ali, &ali, &open).await.unwrap();
        assert_eq!(
            held.get_price_minor().map(LedgerAmount::as_minor),
            Some(1_000)
        );
    }

    /// A cancel that lost its flip may only refund the attempt it was actually
    /// cancelling. Driven against the predicate rather than through `cancel`:
    /// the row is re-read *inside* the call, so the only way it can disagree
    /// with what the flip found is a genuine concurrent write, which no
    /// single-process interleaving (and no in-memory engine) can stage.
    #[tokio::test]
    async fn a_lost_flip_refunds_only_the_attempt_it_released() {
        let (db, menu, _leases) = menu(None).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        let veli = crate::db::class_member::tests::fixture_user(&db, "veli").await;
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;

        let seen = book(&db, &menu, &ali, &ali, &open).await.unwrap();
        let cancelled = cancel(&db, seen.clone(), &open, &ali).await.unwrap();
        // The seat this cancel released: its own attempt, still cancelled.
        assert!(refundable_after_lost_flip(&seen, &cancelled).unwrap());

        // A re-book landed in the window. The row is `booked` again with a live
        // charge against it — refunding that frees money for a seat that is
        // still held, and burns the ledger id its own cancel will need. The
        // caller is refused rather than told it cancelled something.
        let rebooked = book(&db, &menu, &ali, &ali, &open).await.unwrap();
        assert_eq!(rebooked.get_attempt(), seen.get_attempt() + 1);
        assert!(matches!(
            refundable_after_lost_flip(&seen, &rebooked),
            Err(AppError::Conflict(_))
        ));

        // Re-booked *and* cancelled again: that cancel appended the reversal
        // for its own attempt, and this call has nothing left to heal.
        let later = cancel(&db, rebooked, &open, &ali).await.unwrap();
        assert!(!refundable_after_lost_flip(&seen, &later).unwrap());

        // And the ordinary lost race — another cancel of the same attempt got
        // there first — still replays the refund, which is what heals a cancel
        // cut short between the flip and the ledger line.
        let other = book(&db, &menu, &veli, &veli, &open).await.unwrap();
        let freed = cancel(&db, other.clone(), &open, &veli).await.unwrap();
        assert!(refundable_after_lost_flip(&other, &freed).unwrap());
    }

    /// Through the crate's one clock ([`crate::domain::timestamp::Timestamp::today_utc`]),
    /// never `chrono::Utc::now` — the guard under test compares against that
    /// same clock, and a test reading a second one is how a timezone mix gets
    /// back in (`clippy.toml` denies it, tests included).
    fn today() -> String {
        Timestamp::today_utc().format("%Y-%m-%d").to_string()
    }

    fn cutoff(minutes: Option<i64>, serving_minute: Option<i64>) -> MealCutoff {
        MealCutoff {
            minutes,
            slots: vec![MealSlotDef::try_new("lunch", serving_minute).unwrap()],
        }
    }

    /// The charge hole itself: a menu whose day has gone by must take no seat
    /// and mint no ledger line, with the shipped defaults in force.
    #[tokio::test]
    async fn a_past_day_takes_no_seat_and_writes_no_charge() {
        let (db, menu, _leases) = menu_on("2020-01-06", None).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        add_dish(&menu, 4550, &db).await;

        assert!(matches!(
            book(&db, &menu, &ali, &ali, &MealCutoff::default()).await,
            Err(AppError::Conflict(_))
        ));
        assert_eq!(seats(&menu, &db).await, 0);
        let (lines, total) = meal_ledger::list_for_student(&db, &ali, None, 0)
            .await
            .unwrap();
        assert!(
            lines.is_empty() && total == 0,
            "a refused booking bills nothing"
        );
    }

    /// The documented decision the past-day guard must not swallow: today's
    /// menu, on a slot with no serving hour, books exactly as it always did —
    /// with the cutoff knob set *and* unset.
    #[tokio::test]
    async fn todays_menu_still_books_without_a_serving_hour() {
        let (db, menu, _leases) = menu_on(&today(), None).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        book(&db, &menu, &ali, &ali, &cutoff(Some(60), None))
            .await
            .expect("an unset serving hour is an unenforced cutoff");
        assert_eq!(seats(&menu, &db).await, 1);
    }

    /// Cancelling stays open on a day already gone — that is how money already
    /// taken is given back, and it is the same reason manager+ bypasses the
    /// cutoff. The guard binds `book` alone.
    #[tokio::test]
    async fn a_past_day_still_gives_the_seat_and_the_money_back() {
        let (db, menu, _leases) = menu_on("2020-01-06", None).await;
        let ali = crate::db::class_member::tests::fixture_user(&db, "ali").await;
        add_dish(&menu, 4550, &db).await;
        // The seat this student is holding was taken while the day was still
        // ahead — the shape the create-side check cannot reach. A test cannot
        // age a menu (`date` is READONLY), so the seat is placed on an
        // already-past menu by the very transaction `book` places it with.
        let fresh = menu::read(&db, &menu).await.unwrap().unwrap();
        let price = meal_ledger::price_snapshot(&db, &menu).await.unwrap();
        let row = MealBooking {
            menu: menu.clone(),
            student: ali,
            booked_by: ali,
            status: MealBookingStatus::Booked,
            attempt: 1,
            price_minor: price,
            cancelled_at: None,
            created_at: Timestamp::now(),
        };
        let charge = MealLedger::charge_for(&row, &ali);
        let booked = match meal_booking::claim_and_place(
            &db,
            &menu,
            cap::UNLIMITED,
            fresh.get_version(),
            &row,
            price,
            charge.as_ref(),
        )
        .await
        .unwrap()
        {
            cap::Claimed::Made(booking) => booking,
            _ => panic!("the seat was free"),
        };
        assert_eq!(seats(&menu, &db).await, 1);

        assert!(matches!(
            book(&db, &menu, &ali, &ali, &MealCutoff::default()).await,
            Err(AppError::Conflict(_)),
        ));
        let cancelled = cancel(&db, booked, &MealCutoff::default(), &ali)
            .await
            .expect("a cancel on a past day still frees the seat");
        assert_eq!(cancelled.get_status(), MealBookingStatus::Cancelled);
        assert_eq!(seats(&menu, &db).await, 0);
        let (lines, _) = meal_ledger::list_for_student(&db, &ali, None, 0)
            .await
            .unwrap();
        assert_eq!(lines.len(), 2, "the charge and its reversal");
    }
}
