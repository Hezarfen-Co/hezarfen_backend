//! The money workflows around the append-only meal ledger: the booking
//! charge and its reversal (the ledger halves of the seat workflows in
//! [`crate::service::meal_booking`]) and the recorded credit. The rows,
//! id minters and the sign convention live in
//! [`crate::domain::meal_ledger`]; the writes and the SUM/balance reads in
//! [`crate::db::meal_ledger`].

use crate::database::Database;
use crate::db::meal_ledger;
use crate::domain::meal_booking::MealBooking;
use crate::domain::meal_ledger::{
    LedgerAmount, LedgerMethod, LedgerNote, MealLedger, MealLedgerId, MealLedgerKind,
};
use crate::domain::payment_ledger::PaymentRequestKey;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Bill `booking`'s current attempt at the price frozen onto it when that
/// attempt took the seat.
///
/// The price comes off the *booking row*, never off the menu as it stands
/// now: a seat taken while the menu was free carries `None` forever, so
/// "was free" is a recorded fact and a later dish never bills a seat
/// retroactively. Called from
/// [`book`](crate::service::meal_booking::book) alone, right after the seat
/// was claimed, so the seat and its money move together.
///
/// Replaying it is free: the id is `(booking, attempt)`, so a duplicate
/// `POST` writes nothing, and an attempt whose charge failed the first time
/// self-heals on the next `POST` of the same seat.
pub async fn charge_booking(
    db: &Database,
    booking: &MealBooking,
    recorded_by: &UserId,
) -> Result<(), AppError> {
    let Some(line) = MealLedger::charge_for(booking, recorded_by) else {
        return Ok(());
    };
    meal_ledger::append(db, line).await?;
    Ok(())
}

/// Give the money back for a cancelled `booking`: a new `reversal` line for
/// the charge's exact amount, pointing at it. The charge is never touched.
/// A booking that was never billed (a free menu) reverses nothing, and
/// neither does one whose charge never landed — the reversal is keyed to
/// the very charge it undoes, so it can only exist alongside it.
///
/// Keyed by `(booking, attempt)` like the charge, so a retried cancel
/// refunds once. The line normally lands *inside* the flip's own
/// transaction
/// ([`release_seat`](crate::db::meal_booking::release_seat)); this path is what heals a
/// seat flipped before that was true, and it is why
/// [`cancel`](crate::service::meal_booking::cancel) replays it on an already-cancelled row instead
/// of refusing it. Nothing else on the API can append the missing line.
pub async fn reverse_booking(
    db: &Database,
    booking: &MealBooking,
    recorded_by: &UserId,
) -> Result<(), AppError> {
    let Some((charge, line)) = MealLedger::reversal_for(booking, recorded_by) else {
        return Ok(());
    };
    if meal_ledger::read(db, &charge).await?.is_none() {
        return Ok(());
    }
    meal_ledger::append(db, line).await?;
    Ok(())
}

/// Money in: a payment received, or an opening balance.
///
/// With a `request_key` the line is keyed by it (see
/// [`MealLedgerId::for_request`](crate::domain::meal_ledger::MealLedgerId::for_request))
/// and the call is **retry-safe**: a client
/// resending the identical body after a timeout gets back the line the
/// first attempt wrote, not a second credit — nothing on this API can edit
/// or delete one, so a doubled credit is corrected only by a compensating
/// line. Without one the id is a fresh ulid and two identical calls are two
/// credits, which is what a desk taking the same amount twice really means.
pub async fn credit(
    db: &Database,
    student: &UserId,
    amount_minor: LedgerAmount,
    method: Option<LedgerMethod>,
    note: Option<LedgerNote>,
    request_key: Option<&PaymentRequestKey>,
    recorded_by: &UserId,
) -> Result<MealLedger, AppError> {
    let line = meal_ledger::append(
        db,
        MealLedger {
            id: match request_key {
                Some(key) => MealLedgerId::for_request(student, key),
                None => MealLedgerId::generate(),
            },
            student: student.clone(),
            kind: MealLedgerKind::Credit,
            amount_minor,
            source: None,
            method,
            note,
            recorded_by: recorded_by.clone(),
            created_at: Timestamp::now(),
        },
    )
    .await?;
    // A replay is answered from the stored line — but only if it is the
    // same money. The same key for a different amount is a client bug, and
    // handing back the old line would hide it behind a `201`. Checked on
    // what `append` gave back rather than on a read before it: two retries
    // arriving together both find no row, and only the id decides which
    // one's amount is stored, so a check *before* the write would tell the
    // loser its own amount landed.
    if request_key.is_some() && line.get_amount_minor() != amount_minor {
        return Err(AppError::Conflict(
            "this request_key was already used for a different amount",
        ));
    }
    Ok(line)
}

/// A student's whole statement, newest first.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealLedger>, i64), AppError> {
    meal_ledger::list_for_student(db, student, limit, offset).await
}

/// The derived balance: `credits + reversals - charges`, in minor units.
/// Negative means the student owes the school.
pub async fn balance_of(db: &Database, student: &UserId) -> Result<i64, AppError> {
    meal_ledger::balance_of(db, student).await
}
