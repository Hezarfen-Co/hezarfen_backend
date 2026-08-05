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
//! - **Capacity is the menu's `seats_booked` counter**, claimed by a
//!   conditional single-record write ([`cap`]). Counting the rows instead is
//!   write-skew — SurrealDB does not conflict-check a cross-record count
//!   against a concurrent insert — and a process-wide lock around that count
//!   is released around the very round trip the racing insert lands in.
//!   Cancelling gives the seat back in the *same transaction* as the flip.
//! - **The price is claimed, not just read.** The seat is taken at the menu
//!   revision the price was read at, in the same transaction as the row
//!   ([`MealBooking::claim_and_place`]), so a dish re-priced mid-booking loses
//!   the claim and the booking re-reads both. A seat can only ever be billed a
//!   price its menu genuinely carried.
//! - **The seat and its money move together.** Booking charges and cancelling
//!   reverses right here, not in the web layer — a seam between the two let
//!   concurrent `POST`s bill one seat twice — and *both* ledger lines are
//!   written by the very transaction that moves the seat, because money that
//!   trails the seat by one write can be overtaken by the next attempt and
//!   stranded forever: every ledger id carries the attempt it belongs to.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    CAS_UPDATE_RETRIES, MEAL_BOOKING_TABLE, MENU_SEAT_COUNT_FIELD, MENU_VERSION_FIELD,
};
use crate::database::{Database, lost_the_race};
use crate::domain::cap::{self, Claimed};
use crate::domain::meal_ledger::{LedgerAmount, MealLedger};
use crate::domain::menu::{Menu, MenuDate, MenuId, MenuSlot};
use crate::domain::page::PagedList;
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
        menu: &MenuId,
        student: &UserId,
        booked_by: &UserId,
        cutoff: &MealCutoff,
        db: &Database,
    ) -> Result<MealBooking, AppError> {
        let id = MealBookingId::composite(menu, student);
        let seats = menu.record();
        for _ in 0..CAS_UPDATE_RETRIES {
            let fresh = Menu::read(menu, db).await?.ok_or(AppError::NotFound)?;
            check_cutoff(fresh.get_date(), fresh.get_slot(), cutoff)?;
            let existing: Option<MealBooking> = db.select(id.record()).await?;
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
                MealLedger::charge_booking(&held, booked_by, db).await?;
                return Ok(held);
            }
            // Before the seat: an unchargeable menu (dishes summing past the
            // cap) must refuse the booking outright, never leave a
            // booked-but-unbilled row behind.
            let price = MealLedger::price_snapshot(menu, db).await?;
            // The attempt is settled *here*, not by the revival's `attempt + 1`,
            // because the charge id is (seat, attempt) and the charge has to be
            // built before the transaction that writes it. `existing` is either
            // absent or cancelled — a booked row returned above.
            let fresh_row = MealBooking {
                id: id.clone(),
                menu: menu.clone(),
                student: student.clone(),
                booked_by: booked_by.clone(),
                status: MealBookingStatus::Booked,
                attempt: existing.map_or(1, |prior| prior.attempt + 1),
                price_minor: price,
                cancelled_at: None,
                created_at: Timestamp::now(),
            };
            let charge = MealLedger::charge_for(&fresh_row, booked_by);
            match Self::claim_and_place(
                &seats,
                fresh.get_capacity().unwrap_or(cap::UNLIMITED),
                fresh.get_version(),
                &fresh_row,
                price,
                charge.as_ref(),
                db,
            )
            .await?
            {
                // Seat, row and charge committed together; nothing is owed
                // after this call, which is the whole point of folding them.
                Claimed::Made(booking) => return Ok(booking),
                // Another `POST` of this very pair moved the row first, and this
                // caller never took a seat (the whole transaction rolled back).
                // The decision is simply made again rather than answered off a
                // row read here: the winner may have left it `cancelled` at an
                // attempt this call never saw, and the loop's own held-seat path
                // is what returns a live seat *and* replays its charge.
                Claimed::Duplicate => continue,
                // Full, moved, or gone — one `WHERE` refused all three, so the
                // reason is re-read rather than guessed. A moved menu is not a
                // refusal: the price above is stale, so price and seat are
                // taken again together.
                Claimed::Full => match Menu::read(menu, db).await? {
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
    /// [`cap::claim_and_create`], with the revision guard and the revival of a
    /// cancelled row that the general form does not carry.
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
    async fn claim_and_place(
        seats: &RecordId,
        cap: i64,
        seen: i64,
        row: &MealBooking,
        price: Option<LedgerAmount>,
        charge: Option<&MealLedger>,
        db: &Database,
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
        for _ in 0..CAS_UPDATE_RETRIES {
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

    /// Free the seat: flip to `cancelled`, stamp the moment, **and give the
    /// money back**. Refused (409) only once the cutoff has passed.
    ///
    /// The row is re-read here and the flip is conditional on it still being
    /// `booked`: the caller's copy may predate a concurrent re-book, and
    /// cancelling a stale attempt would strand that attempt's charge forever.
    /// The reversal is keyed to the attempt, so a retried cancel refunds once.
    ///
    /// **The refund is not a second write.** It is appended by the very
    /// transaction that flips the row (see [`Self::release_seat`]), so a
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
        self,
        cutoff: &MealCutoff,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<MealBooking, AppError> {
        let fresh = Self::read(&self.id, db).await?.ok_or(AppError::NotFound)?;
        if fresh.status == MealBookingStatus::Cancelled {
            MealLedger::reverse_booking(&fresh, recorded_by, db).await?;
            return Ok(fresh);
        }
        let menu = Menu::read(&fresh.menu, db)
            .await?
            .ok_or(AppError::NotFound)?;
        check_cutoff(menu.get_date(), menu.get_slot(), cutoff)?;
        // Lost the flip: the row is no longer the `booked` attempt this call
        // read — another cancel took the seat back first, or a booking took it
        // again. Its row is the truth, but only the attempt *this* call was
        // cancelling may be refunded off it (see
        // [`Self::refundable_after_lost_flip`]).
        let saved = match Self::release_seat(&fresh, recorded_by, db).await? {
            Some(cancelled) => cancelled,
            None => {
                let live = Self::read(&self.id, db).await?.ok_or(AppError::NotFound)?;
                if !Self::refundable_after_lost_flip(&fresh, &live)? {
                    return Ok(live);
                }
                live
            }
        };
        MealLedger::reverse_booking(&saved, recorded_by, db).await?;
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
    /// reaches here: `cancel` answers an already-cancelled row above.)
    fn refundable_after_lost_flip(
        seen: &MealBooking,
        live: &MealBooking,
    ) -> Result<bool, AppError> {
        if live.status == MealBookingStatus::Booked {
            return Err(AppError::Conflict(
                "the seat was booked again while this cancellation ran",
            ));
        }
        Ok(live.attempt == seen.attempt)
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
    /// The book side is folded the same way ([`Self::claim_and_place`]), so
    /// there is no half-second anywhere in which a seat is held with no charge
    /// against it, or a charge stands with no seat behind it.
    ///
    /// **The flip is fenced on the attempt it read**, exactly as the revival on
    /// the book side is. `status = 'booked'` alone matches a seat somebody else
    /// took in the round trip [`Self::cancel`] spends re-reading the menu: the
    /// flip then cancels attempt N+1, hands its seat back, and refunds nothing
    /// at all — the ids bound here are attempt N's, and N's reversal is already
    /// written — so a `POST` answered `201` a moment ago loses its seat with its
    /// charge standing, and the `(booking, N+1)` reversal id is burnt for good.
    ///
    /// `None` = the row was not this call's own `booked` attempt any more.
    /// Retried while the store reports a write conflict: the menu row is
    /// contended by every booking on it, and that contention is the cap
    /// working, not an error.
    async fn release_seat(
        booked: &MealBooking,
        recorded_by: &UserId,
        db: &Database,
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
        for _ in 0..CAS_UPDATE_RETRIES {
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

    pub async fn read(id: &MealBookingId, db: &Database) -> Result<Option<MealBooking>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every booking on a menu, cancelled ones included — the kitchen's list.
    pub async fn list_for_menu(
        menu: &MenuId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
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
    /// [`ensure_can_observe`](crate::web::ensure_can_observe) refuses. The
    /// caller re-derives the list from the links on every read, so the view
    /// dies with the link.
    pub async fn list_for_students(
        students: &[UserId],
        limit: Option<i64>,
        offset: i64,
        db: &Database,
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
///
/// A date no serving instant can be computed from **fails closed**. A menu on
/// an impossible day (a 31st of February — refused by [`MenuDate`] now, but
/// rows written before that rule are still on the volume) otherwise skipped the
/// deadline entirely, silently and forever, however large the school set it:
/// the one menu with no cutoff at all would be the one nobody meant to publish.
/// Refusing is the only answer that keeps "the deadline binds every menu" true;
/// the menu has to be republished on a real day to become bookable again.
fn check_cutoff(date: &MenuDate, slot: &MenuSlot, cutoff: &MealCutoff) -> Result<(), AppError> {
    // A school with no cutoff configured closes nothing anyway, so an
    // impossible date is not refused there either — that would take the canteen
    // offline for a deadline the school never set.
    let Some(minutes) = cutoff.minutes else {
        return Ok(());
    };
    let Some(serving) = served_at(date, cutoff.serving_minute(slot)) else {
        return Err(AppError::Conflict(
            "the menu's date is not a real calendar day, so its cutoff cannot be worked out",
        ));
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
    use crate::domain::menu_dish::{DishName, DishPrice, DishTags, MenuDish};

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

    /// A published menu on a fresh in-memory database, plus a student.
    async fn menu(capacity: Option<i64>) -> (Database, MenuId) {
        let db = crate::database::init_mem().await.unwrap();
        let slots = vec![MealSlotDef::try_new("lunch", None).unwrap()];
        let menu = Menu::create(
            MenuDate::try_new("2026-09-14").unwrap(),
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

    /// The counter is the cap's authority, so it has to track the seats exactly:
    /// a repeat booking must not count twice, a cancel must hand its seat back
    /// in the same breath as the flip, and a second cancel must not hand back a
    /// seat that was already returned (which would open the cap by one forever).
    #[tokio::test]
    async fn the_seat_counter_follows_the_seats_it_guards() {
        let (db, menu) = menu(Some(1)).await;
        let (ali, veli) = (UserId::generate(), UserId::generate());
        let open = MealCutoff::default();

        let booking = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        assert_eq!(seats(&menu, &db).await, 1);
        // The same seat again is a no-op, not a second claim.
        MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        assert_eq!(seats(&menu, &db).await, 1);
        // …and the cap bites for anyone else while it is held.
        assert!(matches!(
            MealBooking::book(&menu, &veli, &veli, &open, &db).await,
            Err(AppError::Conflict(_))
        ));

        let cancelled = booking.cancel(&open, &ali, &db).await.unwrap();
        assert_eq!(cancelled.get_status(), MealBookingStatus::Cancelled);
        assert_eq!(seats(&menu, &db).await, 0);
        // Cancelling again is idempotent all the way down to the counter.
        cancelled.cancel(&open, &ali, &db).await.unwrap();
        assert_eq!(seats(&menu, &db).await, 0);

        // The freed seat is real, and taking it again counts once.
        let revived = MealBooking::book(&menu, &veli, &veli, &open, &db)
            .await
            .unwrap();
        assert_eq!(revived.get_attempt(), 1);
        assert_eq!(seats(&menu, &db).await, 1);
    }

    /// A menu whose row is gone or whose seats are held refuses its own delete,
    /// in the `WHERE` rather than in a read the delete then trusts.
    #[tokio::test]
    async fn a_held_seat_refuses_the_menu_delete() {
        let (db, id) = menu(None).await;
        let ali = UserId::generate();
        let booking = MealBooking::book(&id, &ali, &ali, &MealCutoff::default(), &db)
            .await
            .unwrap();
        let row = Menu::read(&id, &db).await.unwrap().unwrap();
        assert!(matches!(
            row.clone().delete(&db).await,
            Err(AppError::Conflict(_))
        ));
        booking
            .cancel(&MealCutoff::default(), &ali, &db)
            .await
            .unwrap();
        assert!(row.delete(&db).await.is_ok());
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
            MealBooking::claim_and_place(
                &menu.record(),
                cap::UNLIMITED,
                seen,
                &row,
                None,
                None,
                &db,
            )
            .await
            .unwrap()
        };
        assert!(
            matches!(place(seen).await, Claimed::Full),
            "a claim at a revision the menu has left must be refused"
        );
        assert_eq!(seats(&menu, &db).await, 0, "and must write nothing");
        assert!(
            MealBooking::read(row.get_id(), &db)
                .await
                .unwrap()
                .is_none(),
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

    /// A booking placed after a price edit is billed the new price, and one
    /// placed before keeps the old one — the seat's `price_minor` is history,
    /// never re-read from the menu.
    #[tokio::test]
    async fn a_seat_keeps_the_price_it_was_taken_at() {
        let (db, menu) = menu(None).await;
        let (ali, veli) = (UserId::generate(), UserId::generate());
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;

        let early = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        add_dish(&menu, 500, &db).await;
        let late = MealBooking::book(&menu, &veli, &veli, &open, &db)
            .await
            .unwrap();

        assert_eq!(
            early.get_price_minor().map(LedgerAmount::as_minor),
            Some(1_000)
        );
        assert_eq!(
            late.get_price_minor().map(LedgerAmount::as_minor),
            Some(1_500)
        );
        // The seat taken before the second dish is not re-billed for it.
        let held = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
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
        let (db, menu) = menu(None).await;
        let (ali, veli) = (UserId::generate(), UserId::generate());
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;

        let seen = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        let cancelled = seen.clone().cancel(&open, &ali, &db).await.unwrap();
        // The seat this cancel released: its own attempt, still cancelled.
        assert!(MealBooking::refundable_after_lost_flip(&seen, &cancelled).unwrap());

        // A re-book landed in the window. The row is `booked` again with a live
        // charge against it — refunding that frees money for a seat that is
        // still held, and burns the ledger id its own cancel will need. The
        // caller is refused rather than told it cancelled something.
        let rebooked = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        assert_eq!(rebooked.get_attempt(), seen.get_attempt() + 1);
        assert!(matches!(
            MealBooking::refundable_after_lost_flip(&seen, &rebooked),
            Err(AppError::Conflict(_))
        ));

        // Re-booked *and* cancelled again: that cancel appended the reversal
        // for its own attempt, and this call has nothing left to heal.
        let later = rebooked.cancel(&open, &ali, &db).await.unwrap();
        assert!(!MealBooking::refundable_after_lost_flip(&seen, &later).unwrap());

        // And the ordinary lost race — another cancel of the same attempt got
        // there first — still replays the refund, which is what heals a cancel
        // cut short between the flip and the ledger line.
        let other = MealBooking::book(&menu, &veli, &veli, &open, &db)
            .await
            .unwrap();
        let freed = other.clone().cancel(&open, &veli, &db).await.unwrap();
        assert!(MealBooking::refundable_after_lost_flip(&other, &freed).unwrap());
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

        let stale = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        stale.clone().cancel(&open, &ali, &db).await.unwrap();
        let live = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        assert_eq!(live.get_attempt(), stale.get_attempt() + 1);

        assert!(
            MealBooking::release_seat(&stale, &ali, &db)
                .await
                .unwrap()
                .is_none(),
            "the attempt this call read is gone, so it releases nothing"
        );
        // Stored state, not the returned value: the mem engine forges wins.
        let stored = MealBooking::read(live.get_id(), &db).await.unwrap().unwrap();
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
            MealBooking::book(&menu, &veli, &veli, &open, &db).await,
            Err(AppError::Conflict(_))
        ));
    }

    /// The seat may not come back without the money. Driven against
    /// [`MealBooking::release_seat`] rather than through `cancel`, because what
    /// is under test is precisely what a *crash right after the flip* leaves
    /// behind: appended a write later, the reversal is lost with the process,
    /// and a re-book then moves the row to the next attempt and locks the
    /// `(booking, attempt)` reversal id out for good — the charge is
    /// unreversible by any route on the API.
    #[tokio::test]
    async fn the_seat_cannot_be_freed_without_its_refund() {
        let (db, menu) = menu(None).await;
        let ali = UserId::generate();
        let open = MealCutoff::default();
        add_dish(&menu, 1_000, &db).await;
        let booked = MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();

        // The flip alone — nothing else runs afterwards, as after a crash.
        let freed = MealBooking::release_seat(&booked, &ali, &db)
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
        MealBooking::book(&menu, &ali, &ali, &open, &db)
            .await
            .unwrap();
        assert_eq!(MealLedger::balance_of(&ali, &db).await.unwrap(), -1_000);
    }

    /// The seat may not be held without its money. Driven against
    /// [`MealBooking::claim_and_place`] rather than through `book`, because what
    /// is under test is precisely what the gap between the two writes leaves
    /// behind: a cancel landing there finds no charge to reverse, reverses
    /// nothing, and the charge then lands anyway — a student holding no seat and
    /// owing money, with the `(booking, attempt)` reversal id burnt for good.
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
            MealBooking::claim_and_place(
                &menu.record(),
                cap::UNLIMITED,
                seen,
                &row,
                price,
                charge.as_ref(),
                &db,
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
        let booked = MealBooking::book(&menu, &ali, &ali, &MealCutoff::default(), &db)
            .await
            .unwrap();

        MealBooking::release_seat(&booked, &ali, &db)
            .await
            .unwrap()
            .expect("a free seat is still a seat");
        assert_eq!(seats(&menu, &db).await, 0);
        let (lines, total) = MealLedger::list_for_student(&ali, None, 0, &db)
            .await
            .unwrap();
        assert!(lines.is_empty() && total == 0, "a free seat moves no money");
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

    /// A menu stored on a day that does not exist has no serving instant, so no
    /// deadline can be counted back from it — and it must therefore refuse, not
    /// pass. `MenuDate::try_new` rejects such a date now; this is a row written
    /// before that rule, read back off the store exactly as the domain reads it
    /// (the column is `TYPE string`), which is the only way one can still turn
    /// up. Passing is what let it be booked and cancelled with no cutoff ever.
    #[test]
    fn an_impossible_day_has_no_open_deadline() {
        let impossible = MenuDate::from_value(Value::String("2026-02-29".into())).unwrap();
        assert!(MenuDate::try_new(impossible.as_str()).is_err());
        assert!(check_cutoff(&impossible, &lunch(), &cutoff(Some(60), Some(720))).is_err());
        assert!(check_cutoff(&impossible, &lunch(), &cutoff(Some(60), None)).is_err());
        // A school that set no cutoff closes nothing anyway, impossible day or
        // not: refusing there would take the canteen offline for no deadline.
        assert!(check_cutoff(&impossible, &lunch(), &cutoff(None, Some(720))).is_ok());
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
