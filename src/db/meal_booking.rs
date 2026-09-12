//! The `meal_booking` table: row reads, the two listings, and the folded
//! transactions every seat moves by — [`claim_and_place`] (seat + row +
//! charge in one transaction) and [`release_seat`] (flip + counter + refund
//! in one). The book/cancel workflows that sequence these live in
//! [`crate::service::meal_booking`]; the row shape and the deadline checks
//! in [`crate::domain::meal_booking`].

use crate::database::{Database, tx_with_retry};
use crate::db::cap::Claimed;
use crate::db::page::PagedList;
use crate::domain::meal_booking::{MealBooking, MealBookingId, MealBookingStatus};
use crate::domain::meal_ledger::{LedgerAmount, MealLedger};
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn read(db: &Database, id: &MealBookingId) -> Result<Option<MealBooking>, AppError> {
    let row = sqlx::query_as!(
        MealBooking,
        "SELECT menu AS \"menu: MenuId\", student AS \"student: UserId\", booked_by AS \"booked_by: UserId\", status AS \"status: MealBookingStatus\", attempt, price_minor AS \"price_minor: LedgerAmount\", cancelled_at AS \"cancelled_at: Timestamp\", created_at AS \"created_at: Timestamp\"
         FROM meal_booking WHERE menu = $1 AND student = $2",
        id.menu().key(),
        id.student().uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Every booking on a menu, cancelled ones included — the kitchen's list.
pub async fn list_for_menu(
    db: &Database,
    menu: &MenuId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealBooking>, i64), AppError> {
    PagedList::new(
        "meal_booking WHERE menu = $1",
        "ORDER BY created_at DESC, student DESC",
    )
    .bind(menu.key().to_string())
    .run(limit, offset, db)
    .await
}

/// Every seat held for one of `students`, newest first — a caller's own
/// list is themselves plus whoever they hold a *live* parent link to.
///
/// Deliberately **not** `booked_by`: who placed a booking is history
/// written onto a fixed column, and history is not a read grant. A parent
/// whose link was revoked (by an unlink or by the student-side role sweep)
/// would otherwise keep a live view of the child's seat, watching
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
    if students.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let ids: Vec<uuid::Uuid> = students.iter().map(UserId::uuid).collect();
    let rows = sqlx::query_as!(
        MealBooking,
        "SELECT menu AS \"menu: MenuId\", student AS \"student: UserId\", booked_by AS \"booked_by: UserId\", status AS \"status: MealBookingStatus\", attempt, price_minor AS \"price_minor: LedgerAmount\", cancelled_at AS \"cancelled_at: Timestamp\", created_at AS \"created_at: Timestamp\"
         FROM meal_booking WHERE student = ANY($1::uuid[])
         ORDER BY created_at DESC, menu DESC
         LIMIT $2 OFFSET $3",
        &ids,
        limit,
        offset,
    )
    .fetch_all(db)
    .await?;
    // The window's own total — the same count the paging envelope answers.
    let total = if limit.is_some() || offset != 0 {
        sqlx::query_scalar!(
            "SELECT count(*) FROM meal_booking WHERE student = ANY($1::uuid[])",
            &ids,
        )
        .fetch_one(db)
        .await?
        .unwrap_or(0)
    } else {
        rows.len() as i64
    };
    Ok((rows, total))
}

/// Take the seat **and** place the row in one transaction, at the menu
/// revision the price was read at.
///
/// The seat and the row cannot be two steps. The row key is the (menu,
/// student) pair, so two `POST`s of the same seat both find no row and both
/// claim — and on a tight cap the second is then told "full" for a seat it
/// never owed, while the winner's row may not even be visible yet. Here the
/// duplicate insert answers [`Claimed::Duplicate`] and the caller replays
/// the winner, while the whole transaction (seat bump included) rolls back
/// on any refused path: the counter never counts a row that does not exist.
/// Same shape as the [`crate::db::cap`] claim recipe, with the revision
/// guard and the revival of a cancelled row that the general form does not
/// carry.
///
/// A cancelled row is revived in place (a fresh attempt at `price`) rather
/// than recreated: `booked_by` and `created_at` are fixed history — who
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
/// instead, so a rival that revived first loses this claim (the insert below
/// then finds the row and answers `Duplicate`) rather than billing its
/// attempt at this call's id.
///
/// The charge is created only if it is not already there — an attempt
/// billed twice is the one thing money code may never do.
pub(crate) async fn claim_and_place(
    db: &Database,
    menu: &MenuId,
    cap: i64,
    seen: i64,
    row: &MealBooking,
    price: Option<LedgerAmount>,
    charge: Option<&MealLedger>,
) -> Result<Claimed<MealBooking>, AppError> {
    let menu_key = menu.key().to_string();
    let student = row.student.uuid();
    let attempt = row.attempt;
    // Owned captures only: a closure holding a `&T` fails the higher-ranked
    // `Send` check `tx_with_retry`'s future must pass.
    let booked_by = row.booked_by.uuid();
    let status = row.status.as_str();
    let cancelled_at = row.cancelled_at.map(|t| t.as_millis());
    let created_at = row.created_at.as_millis();
    let charge = charge.map(|line| {
        (
            line.id.key().to_string(),
            line.student.uuid(),
            line.kind.as_str(),
            line.amount_minor.as_minor(),
            line.source.clone(),
            line.method.as_ref().map(|m| m.as_str().to_string()),
            line.note.as_ref().map(|n| n.as_str().to_string()),
            line.recorded_by.uuid(),
            line.created_at.as_millis(),
        )
    });
    tx_with_retry(db, false, async move |tx| {
        // Already held? Same attempt, same price it was taken at — answered
        // as `Duplicate`, which the caller replays into the winner's row
        // (and its charge) without claiming anything.
        let held = sqlx::query!(
            "SELECT 1 AS held FROM meal_booking
             WHERE menu = $1 AND student = $2 AND status = 'booked'",
            menu_key,
            student,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if held.is_some() {
            return Ok(Claimed::Duplicate);
        }
        // The seat: one conditional single-row write on the menu, atomic
        // under Postgres — of the racers only `cap` of them per revision
        // get a non-empty result, and the rest are refused with nothing
        // written.
        let seat = sqlx::query!(
            "UPDATE menu SET seats_booked = seats_booked + 1
             WHERE id = $1 AND seats_booked < $2 AND COALESCE(version, 0) = $3
             RETURNING 1 AS seat",
            menu_key,
            cap,
            seen,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if seat.is_none() {
            return Ok(Claimed::Full);
        }
        // Revive a cancelled row only while it still stands at the attempt
        // this call read — the fence that keeps a rival's revival from
        // being billed at this call's ids.
        let revived = sqlx::query_as!(
            MealBooking,
            "UPDATE meal_booking
             SET status = 'booked', attempt = $3, price_minor = $4, cancelled_at = NULL
             WHERE menu = $1 AND student = $2
               AND status = 'cancelled' AND attempt = $3::bigint - 1
             RETURNING menu AS \"menu: MenuId\", student AS \"student: UserId\", booked_by AS \"booked_by: UserId\", status AS \"status: MealBookingStatus\", attempt, price_minor AS \"price_minor: LedgerAmount\", cancelled_at AS \"cancelled_at: Timestamp\", created_at AS \"created_at: Timestamp\"",
            menu_key,
            student,
            attempt,
            price.map(LedgerAmount::as_minor),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let placed = match revived {
            Some(row) => Some(row),
            None => {
                // No row (or a rival moved it past this attempt): place a
                // fresh one. A rival that landed first answers `Duplicate`
                // — nothing here overwrites its row.
                let inserted = sqlx::query_as!(
                    MealBooking,
                    "INSERT INTO meal_booking
                         (menu, student, booked_by, status, attempt, price_minor, cancelled_at, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                     ON CONFLICT (menu, student) DO NOTHING
                     RETURNING menu AS \"menu: MenuId\", student AS \"student: UserId\", booked_by AS \"booked_by: UserId\", status AS \"status: MealBookingStatus\", attempt, price_minor AS \"price_minor: LedgerAmount\", cancelled_at AS \"cancelled_at: Timestamp\", created_at AS \"created_at: Timestamp\"",
                    menu_key,
                    student,
                    booked_by,
                    status,
                    attempt,
                    price.map(LedgerAmount::as_minor),
                    cancelled_at,
                    created_at,
                )
                .fetch_optional(&mut *tx)
                .await?;
                inserted
            }
        };
        let Some(placed) = placed else {
            return Ok(Claimed::Duplicate);
        };
        // The charge rides the same transaction; already there (a replayed
        // POST of this seat) means fine — nothing is written twice.
        if let Some((id, student, kind, amount, source, method, note, recorded_by, created_at)) =
            &charge
        {
            sqlx::query!(
                "INSERT INTO meal_ledger
                     (id, student, kind, amount_minor, source, method, note, recorded_by, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (id) DO NOTHING",
                id.as_str(),
                student,
                kind,
                amount,
                source.as_deref(),
                method.as_deref(),
                note.as_deref(),
                recorded_by,
                created_at,
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(Claimed::Made(placed))
    })
    .await
}

/// Flip the seat to `cancelled` and give it back to the menu's counter **in
/// one transaction**, so the two can never disagree: a decrement that ran
/// without the flip frees a seat still held, and a flip without the
/// decrement locks a seat nothing can ever release. The decrement rides the
/// flip's own result, so a row already cancelled — by a racing cancel, or
/// by this call retried — decrements nothing.
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
pub async fn release_seat(
    db: &Database,
    booked: &MealBooking,
    recorded_by: &UserId,
) -> Result<Option<MealBooking>, AppError> {
    let refund = MealLedger::reversal_for(booked, recorded_by);
    let menu_key = booked.menu.key().to_string();
    let student = booked.student.uuid();
    let attempt = booked.attempt;
    let cancelled_at = Timestamp::now();
    tx_with_retry(db, false, async move |tx| {
        let flipped = sqlx::query_as!(
            MealBooking,
            "UPDATE meal_booking SET status = 'cancelled', cancelled_at = $3
             WHERE menu = $1 AND student = $2 AND status = 'booked' AND attempt = $4
             RETURNING menu AS \"menu: MenuId\", student AS \"student: UserId\", booked_by AS \"booked_by: UserId\", status AS \"status: MealBookingStatus\", attempt, price_minor AS \"price_minor: LedgerAmount\", cancelled_at AS \"cancelled_at: Timestamp\", created_at AS \"created_at: Timestamp\"",
            menu_key,
            student,
            cancelled_at.as_millis(),
            attempt,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if flipped.is_some() {
            sqlx::query!(
                "UPDATE menu SET seats_booked = GREATEST(seats_booked - 1, 0) WHERE id = $1",
                menu_key,
            )
            .execute(&mut *tx)
            .await?;
        }
        // Guarded three ways: nothing was flipped (a rival cancelled first,
        // and its own transaction carried the refund), the charge never
        // landed, or this attempt is already refunded — none of which may
        // abort the flip.
        if let (true, Some((charge, line))) = (flipped.is_some(), &refund) {
            let charge_there = sqlx::query!(
                "SELECT 1 AS there FROM meal_ledger WHERE id = $1",
                charge.key(),
            )
            .fetch_optional(&mut *tx)
            .await?;
            let reversal_there = sqlx::query!(
                "SELECT 1 AS there FROM meal_ledger WHERE id = $1",
                line.id.key(),
            )
            .fetch_optional(&mut *tx)
            .await?;
            if charge_there.is_some() && reversal_there.is_none() {
                sqlx::query!(
                    "INSERT INTO meal_ledger
                         (id, student, kind, amount_minor, source, method, note, recorded_by, created_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (id) DO NOTHING",
                    line.id.key(),
                    line.student.uuid(),
                    line.kind.as_str(),
                    line.amount_minor.as_minor(),
                    line.source.as_deref(),
                    line.method.as_ref().map(|m| m.as_str()),
                    line.note.as_ref().map(|n| n.as_str()),
                    line.recorded_by.uuid(),
                    line.created_at.as_millis(),
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        Ok(flipped)
    })
    .await
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::db::cap;
    use crate::db::meal_ledger;
    use crate::db::menu;
    use crate::db::menu_dish;
    use crate::domain::meal_booking::{MealBookingStatus, MealCutoff};
    use crate::domain::menu::{MenuDate, MenuSlot};
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
        let menu = menu::create(
            &db,
            MenuDate::try_new(date).unwrap(),
            MenuSlot::try_new("lunch", &slots).unwrap(),
            capacity,
            &UserId::generate(),
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
        menu_dish::create(
            db,
            menu,
            DishName::try_new("çorba").unwrap(),
            None,
            DishPrice::try_new(price).unwrap(),
            DishTags::try_new(&[], &[]).unwrap(),
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
        let seen = menu::read(&db, &menu).await.unwrap().unwrap().get_version();
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
        let now = menu::read(&db, &menu).await.unwrap().unwrap().get_version();
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
            let now = menu::read(db, &id).await.unwrap().unwrap().get_version();
            let stepped = now > seen;
            seen = now;
            stepped
        };
        assert!(!moved(&db).await, "a fresh menu starts where it starts");

        let dish = add_dish(&id, 1_000, &db).await;
        assert!(moved(&db).await, "a dish added");
        let dish = menu_dish::update(
            &db,
            dish,
            None,
            None,
            Some(DishPrice::try_new(2_000).unwrap()),
            None,
        )
        .await
        .unwrap();
        assert!(moved(&db).await, "a dish re-priced");
        menu_dish::delete(&db, dish).await.unwrap();
        assert!(moved(&db).await, "a dish removed");
        menu::update(
            &db,
            menu::read(&db, &id).await.unwrap().unwrap(),
            Some(Some(5)),
        )
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
            meal_ledger::balance_of(&db, &ali).await.unwrap(),
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
            meal_ledger::balance_of(&db, &ali).await.unwrap(),
            0,
            "the reversal must have committed with the flip, not after it"
        );

        // And the re-book that used to strand the charge now finds it settled:
        // a fresh attempt, billed once more, with the old one squared away.
        meal_booking::book(&db, &menu, &ali, &ali, &open)
            .await
            .unwrap();
        assert_eq!(meal_ledger::balance_of(&db, &ali).await.unwrap(), -1_000);
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
        let price = meal_ledger::price_snapshot(&db, &menu).await.unwrap();
        let seen = menu::read(&db, &menu).await.unwrap().unwrap().get_version();
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
            meal_ledger::balance_of(&db, &ali).await.unwrap(),
            -1_000,
            "the charge must have committed with the claim, not after it"
        );

        // Replayed: the seat is already held, so the whole transaction rolls
        // back — no second seat and, just as importantly, no second charge.
        assert!(matches!(place().await, Claimed::Duplicate));
        assert_eq!(seats(&menu, &db).await, 1);
        assert_eq!(meal_ledger::balance_of(&db, &ali).await.unwrap(), -1_000);
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
        let (lines, total) = meal_ledger::list_for_student(&db, &ali, None, 0)
            .await
            .unwrap();
        assert!(lines.is_empty() && total == 0, "a free seat moves no money");
    }
}
