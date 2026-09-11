//! The `meal_booking` table: row reads, the two listings, and the folded
//! transactions every seat moves by — [`claim_and_place`] (seat + row + charge
//! in one `BEGIN…COMMIT`) and [`release_seat`] (flip + counter + refund in
//! one). The book/cancel workflows that sequence these live in
//! [`crate::service::meal_booking`]; the row shape and the deadline checks in
//! [`crate::domain::meal_booking`].

use surrealdb::types::RecordId;

use crate::constant::{CAP_WRITE_TRIES, MENU_SEAT_COUNT_FIELD, MENU_VERSION_FIELD};
use crate::database::{Database, backoff, lost_the_race};
use crate::db::cap::Claimed;
use crate::db::page::PagedList;
use crate::domain::meal_booking::{MealBooking, MealBookingId};
use crate::domain::meal_ledger::{LedgerAmount, MealLedger};
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn read(db: &Database, id: &MealBookingId) -> Result<Option<MealBooking>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Every booking on a menu, cancelled ones included — the kitchen's list.
pub async fn list_for_menu(
    db: &Database,
    menu: &MenuId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealBooking>, i64), AppError> {
    PagedList::new(
        "meal_booking WHERE menu = $menu",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind("menu", menu.record())
    .run(limit, offset, db)
    .await
}

/// Every seat held for one of `students`, newest first — a caller's own
/// list is themselves plus whoever they hold a *live* parent link to.
///
/// Deliberately **not** `booked_by = $usr`: who placed a booking is history
/// written onto a READONLY column, and history is not a read grant. A
/// parent whose link was revoked (by an unlink or by the student-side role
/// sweep) would otherwise keep a live view of the child's seat, watching
/// cancellations made long after the link died — exactly what
/// [`ensure_can_observe`](crate::service::parent_link::ensure_can_observe) refuses. The
/// caller re-derives the list from the links on every read, so the view
/// dies with the link.
pub async fn list_for_students(
    db: &Database,
    students: &[UserId],
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealBooking>, i64), AppError> {
    PagedList::new(
        "meal_booking WHERE student IN $students",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind(
        "students",
        students.iter().map(UserId::record).collect::<Vec<_>>(),
    )
    .run(limit, offset, db)
    .await
}

/// Take the seat **and** place the row in one transaction, at the menu
/// revision the price was read at.
///
/// The seat and the row cannot be two steps. The id is the (menu, student)
/// pair, so two `POST`s of the same seat both find no row and both claim —
/// and on a tight cap the second is then told "full" for a seat it never
/// owed, while the winner's row may not even be visible yet. Here the
/// duplicate `CREATE` aborts the transaction, which takes its increment with
/// it: the counter never counts a row that does not exist, and a loser is
/// answered with the winner's booking instead of a refusal. Same shape as
/// [`cap::claim_and_create`](crate::db::cap::claim_and_create), with the
/// revision guard and the revival of a cancelled row that the general form
/// does not carry.
///
/// A cancelled row is revived in place (a fresh attempt at `price`) rather
/// than recreated: `booked_by` and `created_at` are READONLY history — who
/// opened the seat, and when.
///
/// **The charge rides this transaction too**, for the same reason the
/// refund rides the cancel's: appended a write later, a cancel landing in
/// the gap finds no charge, reverses nothing, and the charge lands after it
/// — the seat given back and the money still owed, unreversible by any
/// route, because every ledger id carries the attempt the cancel has already
/// moved past. That is why `row.attempt` is minted by the *caller*: the
/// charge's id is (seat, attempt), so the number cannot be the revival's own
/// `attempt + 1` any more. The revival is fenced on the attempt it read
/// instead, so a rival that revived first loses this claim (the `CREATE`
/// below then finds the row and aborts the whole transaction) rather than
/// billing its attempt at this call's id.
///
/// The charge is created only if it is not already there — a duplicate
/// `CREATE` would abort the transaction and cost the seat, and an attempt
/// billed twice is the one thing money code may never do.
pub(crate) async fn claim_and_place(
    db: &Database,
    seats: &RecordId,
    cap: i64,
    seen: i64,
    row: &MealBooking,
    price: Option<LedgerAmount>,
    charge: Option<&MealLedger>,
) -> Result<Claimed<MealBooking>, AppError> {
    // Empty when the menu is free: a seat that costs nothing owes no line.
    let bill = match charge {
        Some(_) => {
            "IF array::len((SELECT VALUE id FROM $charge)) = 0 \
                 { CREATE $charge CONTENT $line };"
        }
        None => "",
    };
    // Slots count BEGIN, three LETs, three IFs and — when there is money to
    // take — the charge's IF, so the RETURN is slot 7 or 8.
    let returned = if charge.is_some() { 8 } else { 7 };
    // Parenthesized `??` throughout: `a ?? 0 = $seen` binds the wrong way.
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $held = (SELECT VALUE id FROM $id WHERE status = 'booked');
         IF array::len($held) > 0 {{ THROW 'held' }};
         LET $seat = (UPDATE $menu SET {MENU_SEAT_COUNT_FIELD} = \
             ({MENU_SEAT_COUNT_FIELD} ?? 0) + 1 \
             WHERE ({MENU_SEAT_COUNT_FIELD} ?? 0) < $cap \
               AND ({MENU_VERSION_FIELD} ?? 0) = $seen RETURN VALUE id);
         IF array::len($seat) = 0 {{ THROW 'no_seat' }};
         LET $revived = (UPDATE $id SET status = 'booked', attempt = $attempt, \
             price_minor = $price, cancelled_at = NONE \
             WHERE status = 'cancelled' AND attempt = $attempt - 1 RETURN AFTER);
         IF array::len($revived) = 0 {{ CREATE $id CONTENT $row }};
         {bill}
         RETURN SELECT * FROM ONLY $id;
         COMMIT TRANSACTION;"
    );
    // Every booking on one menu contends on that menu's single counter row,
    // which is one HTTP burst of racers on a single parent — the case
    // `CAP_WRITE_TRIES` is sized for. Three immediate re-sends with no
    // backoff just re-synchronized them and 409'd the fourth student.
    // Admissible: a lost round aborts the whole transaction (nothing
    // written, no seat, no charge), and the two `CREATE`s that could
    // legitimately answer "already exists" are read as `Claimed::Duplicate`
    // below *before* the conflict check, so no decision is ever re-asked.
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let query = db
            .query(sql.as_str())
            .bind(("menu", seats.clone()))
            .bind(("cap", cap))
            .bind(("seen", seen))
            .bind(("id", row.id.record()))
            .bind(("attempt", row.attempt))
            .bind(("price", price))
            .bind(("row", row.clone()));
        let query = match charge {
            Some(line) => query
                .bind(("charge", line.get_id().record()))
                .bind(("line", line.clone())),
            None => query,
        };
        let mut result = match query.await {
            Ok(result) => result,
            Err(err) if lost_the_race(&err) => continue,
            Err(err) => return Err(err.into()),
        };
        // An aborted transaction errors *every* slot, most with a generic
        // "not executed" — only the failing slot says why, so scan them all.
        let mut errors = result.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains("no_seat"))
        {
            return Ok(Claimed::Full);
        }
        if errors
            .values()
            .any(|error| error.to_string().contains("held") || error.is_already_exists())
        {
            return Ok(Claimed::Duplicate);
        }
        if errors.values().any(lost_the_race) {
            continue;
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        return match result.take::<Option<MealBooking>>(returned)? {
            Some(placed) => Ok(Claimed::Made(placed)),
            None => Err(AppError::Internal("failed to book the meal".into())),
        };
    }
    Err(AppError::Conflict(
        "the menu kept changing underneath this booking",
    ))
}

/// Flip the seat to `cancelled` and give it back to the menu's counter **in
/// one transaction**, so the two can never disagree: a decrement that ran
/// without the flip frees a seat still held, and a flip without the
/// decrement locks a seat nothing can ever release. The counter follows the
/// flip's own result (`array::len`), so a row already cancelled — by a
/// racing cancel, or by this call retried — decrements nothing.
///
/// **The refund rides this transaction too**, and it has to: the flip and a
/// reversal appended after it are two writes, and a crash in between leaves
/// the seat free with the charge standing — after which a re-book moves the
/// row to attempt N+1 and *no* route can ever write the `(booking, N)`
/// reversal, because the money is keyed to the attempt. Folded in here the
/// seat cannot come back without the money, the same way the counter cannot
/// move without the flip. The reversal is written only when the charge it
/// undoes is really there (checked in the transaction, so no round trip can
/// invalidate it): a refund with no charge behind it invents money.
///
/// The book side is folded the same way ([`claim_and_place`]), so there is
/// no half-second anywhere in which a seat is held with no charge against
/// it, or a charge stands with no seat behind it.
///
/// **The flip is fenced on the attempt it read**, exactly as the revival on
/// the book side is. `status = 'booked'` alone matches a seat somebody else
/// took in the round trip [`crate::service::meal_booking::cancel`] spends
/// re-reading the menu: the flip then cancels attempt N+1, hands its seat
/// back, and refunds nothing at all — the ids bound here are attempt N's,
/// and N's reversal is already written — so a `POST` answered `201` a moment
/// ago loses its seat with its charge standing, and the `(booking, N+1)`
/// reversal id is burnt for good.
///
/// `None` = the row was not this call's own `booked` attempt any more.
/// Retried while the store reports a write conflict: the menu row is
/// contended by every booking on it, and that contention is the cap
/// working, not an error.
pub async fn release_seat(
    db: &Database,
    booked: &MealBooking,
    recorded_by: &UserId,
) -> Result<Option<MealBooking>, AppError> {
    let refund = MealLedger::reversal_for(booked, recorded_by);
    // Guarded three ways: nothing was flipped (a rival cancelled first, and
    // its own transaction carried the refund), the charge never landed, or
    // this attempt is already refunded — none of which may abort the flip.
    let reverse = match refund {
        Some(_) => {
            "IF array::len($flipped) > 0 \
                 AND array::len((SELECT VALUE id FROM $charge)) > 0 \
                 AND array::len((SELECT VALUE id FROM $reversal)) = 0 \
                 { CREATE $reversal CONTENT $line };"
        }
        None => "",
    };
    // Slots count BEGIN, the LET, the seat UPDATE and — when there is money
    // to give back — the IF.
    let returned = if refund.is_some() { 4 } else { 3 };
    // Same burst on the same counter as the book side, so the same patience
    // (see [`claim_and_place`]) — a cancel that gave up left the seat
    // held with the charge standing. Admissible: the round aborts having
    // written nothing, and a re-send finds the row already `cancelled`, so
    // the flip matches nothing, the counter moves nothing and the guarded
    // reversal — keyed to (seat, attempt) — is not written twice.
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let attempted = async {
            let query = db
                .query(format!(
                    "BEGIN TRANSACTION;
                     LET $flipped = (UPDATE $id SET status = 'cancelled', \
                         cancelled_at = $now \
                         WHERE status = 'booked' AND attempt = $attempt RETURN AFTER);
                     UPDATE $menu SET {MENU_SEAT_COUNT_FIELD} = math::max([\
                         ({MENU_SEAT_COUNT_FIELD} ?? 0) - array::len($flipped), 0]);
                     {reverse}
                     RETURN $flipped;
                     COMMIT TRANSACTION;"
                ))
                .bind(("id", booked.id.record()))
                .bind(("menu", booked.menu.record()))
                .bind(("attempt", booked.attempt))
                .bind(("now", Timestamp::now()));
            let query = match &refund {
                Some((charge, line)) => query
                    .bind(("charge", charge.record()))
                    .bind(("reversal", line.get_id().record()))
                    .bind(("line", line.clone())),
                None => query,
            };
            let mut result = query.await?.check()?;
            result.take::<Vec<MealBooking>>(returned)
        }
        .await;
        match attempted {
            Ok(rows) => return Ok(rows.into_iter().next()),
            Err(err) if lost_the_race(&err) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Err(AppError::Conflict(
        "the menu kept changing underneath this cancellation",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::db::cap;
    use crate::domain::meal_booking::{MealBookingStatus, MealCutoff};
    use crate::domain::menu::{Menu, MenuDate, MenuSlot};
    use crate::domain::menu_dish::{DishName, DishPrice, DishTags, MenuDish};
    use crate::domain::settings::MealSlotDef;
    use crate::service::meal_booking;

    /// A published menu on a fresh in-memory database, plus a student. Dated
    /// well ahead on purpose: a menu whose day has passed refuses every
    /// booking, so a date the calendar overtakes would fail this whole module.
    async fn menu(capacity: Option<i64>) -> (Database, MenuId) {
        menu_on("2099-09-14", capacity).await
    }

    async fn menu_on(date: &str, capacity: Option<i64>) -> (Database, MenuId) {
        let db = init_mem().await.unwrap();
        let slots = vec![MealSlotDef::try_new("lunch", None).unwrap()];
        let menu = Menu::create(
            MenuDate::try_new(date).unwrap(),
            MenuSlot::try_new("lunch", &slots).unwrap(),
            capacity,
            &UserId::generate(),
            &db,
        )
        .await
        .unwrap();
        (db, menu.get_id().clone())
    }

    /// The stored counter, absent reading as zero — the number the cap's
    /// `WHERE` actually compares, not one recomputed from the rows.
    async fn seats(menu: &MenuId, db: &Database) -> i64 {
        let mut result = db
            .query("SELECT VALUE seats_booked FROM $id")
            .bind(("id", menu.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<Option<i64>>>(0)
            .unwrap()
            .into_iter()
            .next()
            .flatten()
            .unwrap_or(0)
    }

    async fn add_dish(menu: &MenuId, price: i64, db: &Database) -> MenuDish {
        MenuDish::create(
            menu,
            DishName::try_new("çorba").unwrap(),
            None,
            DishPrice::try_new(price).unwrap(),
            DishTags::try_new(&[], &[]).unwrap(),
            db,
        )
        .await
        .unwrap()
    }

    /// The price a seat is billed must be one the menu carried *at the moment
    /// the seat was taken*. The claim carries the revision the price was read
    /// at, so a dish written in between refuses it — this is that refusal,
    /// driven by hand because no single-process interleaving can produce it.
    #[tokio::test]
    async fn a_seat_cannot_be_claimed_at_a_revision_the_menu_has_left() {
        let (db, menu) = menu(None).await;
        let ali = UserId::generate();
        let seen = Menu::read(&menu, &db).await.unwrap().unwrap().get_version();
        // A dish lands: the price a booking read a moment ago is now stale.
        add_dish(&menu, 1_000, &db).await;
        let row = MealBooking {
            id: MealBookingId::composite(&menu, &ali),
            menu: menu.clone(),
            student: ali.clone(),
            booked_by: ali,
            status: MealBookingStatus::Booked,
            attempt: 1,
            price_minor: None,
            cancelled_at: None,
            created_at: Timestamp::now(),
        };
        let place = async |seen| {
            claim_and_place(&db, &menu.record(), cap::UNLIMITED, seen, &row, None, None)
                .await
                .unwrap()
        };
        assert!(
            matches!(place(seen).await, Claimed::Full),
            "a claim at a revision the menu has left must be refused"
        );
        assert_eq!(seats(&menu, &db).await, 0, "and must write nothing");
        assert!(
            read(&db, row.get_id()).await.unwrap().is_none(),
            "the refused transaction must not leave the row behind either"
        );

        // Re-read, and the same seat is taken with the row it belongs to.
        let now = Menu::read(&menu, &db).await.unwrap().unwrap().get_version();
        assert!(now > seen);
        assert!(matches!(place(now).await, Claimed::Made(_)));
        assert_eq!(seats(&menu, &db).await, 1);
        // …and a second placement of that very row owes no second seat.
        assert!(matches!(place(now).await, Claimed::Duplicate));
        assert_eq!(
            seats(&menu, &db).await,
            1,
            "a duplicate rolls its seat back"
        );
    }

    /// Every write that can move what a seat costs moves the revision — the
    /// half of the rule the CAS above cannot enforce by itself. A dish added,
    /// re-priced or removed, and the capacity moved: each must leave the menu at
    /// a revision no in-flight booking is still holding.
    #[tokio::test]
    async fn every_menu_write_that_moves_the_price_moves_the_revision() {
        let (db, id) = menu(None).await;
        let mut seen = 0;
        let mut moved = async |db: &Database| {
            let now = Menu::read(&id, db).await.unwrap().unwrap().get_version();
            let stepped = now > seen;
            seen = now;
            stepped
        };
        assert!(!moved(&db).await, "a fresh menu starts where it starts");

        let dish = add_dish(&id, 1_000, &db).await;
        assert!(moved(&db).await, "a dish added");
        let dish = dish
            .update(
                None,
                None,
                Some(DishPrice::try_new(2_000).unwrap()),
                None,
                &db,
            )
            .await
            .unwrap();
        assert!(moved(&db).await, "a dish re-priced");
        dish.delete(&db).await.unwrap();
        assert!(moved(&db).await, "a dish removed");
        Menu::read(&id, &db)
            .await
            .unwrap()
            .unwrap()
            .update(Some(Some(5)), &db)
            .await
            .unwrap();
        assert!(moved(&db).await, "the capacity moved");
    }

    /// A cancel may only ever release the attempt it read. The stale handle
    /// here is what a cancel holds across its own `Menu::read` round trip: by
    /// the time the flip runs, that seat has been cancelled and taken again, so
    /// the flip must refuse — unfenced it cancelled the *new* attempt, freeing a
    /// seat the API had just answered `201` for, without refunding it (the
    /// reversal is keyed to the stale attempt, which is already refunded) and
    /// burning the new attempt's own reversal id for good.
    #[tokio::test]
    async fn a_cancel_cannot_release_an_attempt_it_never_read() {
        let (db, menu) = menu(Some(1)).await;
        let (ali, veli) = (UserId::generate(), UserId::generate());
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;

        let stale = meal_booking::book(&db, &menu, &ali, &ali, &open)
            .await
            .unwrap();
        meal_booking::cancel(&db, stale.clone(), &open, &ali)
            .await
            .unwrap();
        let live = meal_booking::book(&db, &menu, &ali, &ali, &open)
            .await
            .unwrap();
        assert_eq!(live.get_attempt(), stale.get_attempt() + 1);

        assert!(
            release_seat(&db, &stale, &ali).await.unwrap().is_none(),
            "the attempt this call read is gone, so it releases nothing"
        );
        // Stored state, not the returned value: the mem engine forges wins.
        let stored = read(&db, live.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_status(), MealBookingStatus::Booked);
        assert_eq!(stored.get_attempt(), live.get_attempt());
        assert_eq!(seats(&menu, &db).await, 1, "and gives no seat back");
        assert_eq!(
            MealLedger::balance_of(&ali, &db).await.unwrap(),
            -1_000,
            "the live attempt's charge stands: it was never cancelled"
        );
        // The seat is still really held — the cap says so too.
        assert!(matches!(
            meal_booking::book(&db, &menu, &veli, &veli, &open).await,
            Err(AppError::Conflict(_))
        ));
    }

    /// The seat may not come back without the money. Driven against
    /// [`release_seat`] rather than through
    /// [`crate::service::meal_booking::cancel`], because what is under test is
    /// precisely what a *crash right after the flip* leaves behind: appended a
    /// write later, the reversal is lost with the process, and a re-book then
    /// moves the row to the next attempt and locks the `(booking, attempt)`
    /// reversal id out for good — the charge is unreversible by any route on
    /// the API.
    #[tokio::test]
    async fn the_seat_cannot_be_freed_without_its_refund() {
        let (db, menu) = menu(None).await;
        let ali = UserId::generate();
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;
        let booked = meal_booking::book(&db, &menu, &ali, &ali, &open)
            .await
            .unwrap();

        // The flip alone — nothing else runs afterwards, as after a crash.
        let freed = release_seat(&db, &booked, &ali)
            .await
            .unwrap()
            .expect("the seat was held, so the flip must take it back");
        assert_eq!(freed.get_status(), MealBookingStatus::Cancelled);
        assert_eq!(seats(&menu, &db).await, 0);
        assert_eq!(
            MealLedger::balance_of(&ali, &db).await.unwrap(),
            0,
            "the reversal must have committed with the flip, not after it"
        );

        // And the re-book that used to strand the charge now finds it settled:
        // a fresh attempt, billed once more, with the old one squared away.
        meal_booking::book(&db, &menu, &ali, &ali, &open)
            .await
            .unwrap();
        assert_eq!(MealLedger::balance_of(&ali, &db).await.unwrap(), -1_000);
    }

    /// The seat may not be held without its money. Driven against
    /// [`claim_and_place`] rather than through
    /// [`crate::service::meal_booking::book`], because what is under test is
    /// precisely what the gap between the two writes leaves behind: a cancel
    /// landing there finds no charge to reverse, reverses nothing, and the
    /// charge then lands anyway — a student holding no seat and owing money,
    /// with the `(booking, attempt)` reversal id burnt for good.
    #[tokio::test]
    async fn the_seat_cannot_be_claimed_without_its_charge() {
        let (db, menu) = menu(None).await;
        let ali = UserId::generate();
        add_dish(&menu, 1_000, &db).await;
        let price = MealLedger::price_snapshot(&menu, &db).await.unwrap();
        let seen = Menu::read(&menu, &db).await.unwrap().unwrap().get_version();
        let row = MealBooking {
            id: MealBookingId::composite(&menu, &ali),
            menu: menu.clone(),
            student: ali.clone(),
            booked_by: ali.clone(),
            status: MealBookingStatus::Booked,
            attempt: 1,
            price_minor: price,
            cancelled_at: None,
            created_at: Timestamp::now(),
        };
        let charge = MealLedger::charge_for(&row, &ali);
        // The claim alone — nothing else runs afterwards, as after a crash.
        let place = async || {
            claim_and_place(
                &db,
                &menu.record(),
                cap::UNLIMITED,
                seen,
                &row,
                price,
                charge.as_ref(),
            )
            .await
            .unwrap()
        };
        assert!(matches!(place().await, Claimed::Made(_)));
        assert_eq!(seats(&menu, &db).await, 1);
        assert_eq!(
            MealLedger::balance_of(&ali, &db).await.unwrap(),
            -1_000,
            "the charge must have committed with the claim, not after it"
        );

        // Replayed: the seat is already held, so the whole transaction rolls
        // back — no second seat and, just as importantly, no second charge.
        assert!(matches!(place().await, Claimed::Duplicate));
        assert_eq!(seats(&menu, &db).await, 1);
        assert_eq!(MealLedger::balance_of(&ali, &db).await.unwrap(), -1_000);
    }

    /// A free menu owes no refund, so the flip must carry no ledger line at all
    /// — the branch the reversal's conditional statement adds.
    #[tokio::test]
    async fn a_seat_that_was_never_billed_frees_without_a_line() {
        let (db, menu) = menu(None).await;
        let ali = UserId::generate();
        let booked = meal_booking::book(&db, &menu, &ali, &ali, &MealCutoff::default())
            .await
            .unwrap();

        release_seat(&db, &booked, &ali)
            .await
            .unwrap()
            .expect("a free seat is still a seat");
        assert_eq!(seats(&menu, &db).await, 0);
        let (lines, total) = MealLedger::list_for_student(&ali, None, 0, &db)
            .await
            .unwrap();
        assert!(lines.is_empty() && total == 0, "a free seat moves no money");
    }
}
