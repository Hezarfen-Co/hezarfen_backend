//! The money workflows: recording a payment, handing one back, and undoing a
//! mistaken line, each under the lock the over-payment cap is built on.
//!
//! **A payment or a refund is replayable when the client names it.** An
//! optional `request_key` keys the line `<target>_k_<key>` (or `_kr_` for a
//! refund), so a retry after a timeout derives the row the first attempt
//! wrote — same identity rule, no scan. The key is scoped by the line it
//! targets, so it never has to be globally unique. Without a key the id is a
//! fresh ulid and two identical calls are two payments, which is what a desk
//! taking the same amount twice really means. The **replay read happens
//! before the cap check** and inside `PAYMENT_LOCK`: the first attempt's
//! line is already inside what the cap folds, so checking the cap first would
//! refuse the very payment that landed. A key replayed with a *different*
//! amount or against a *different* line is a `409`, never the stored line —
//! returning it would hide a client bug behind a `201`.
//!
//! **The over-payment cap is the lock's, not the database's.** [`credit`]
//! and [`refund`] refuse to take more than the line they target
//! is worth, by folding that line's whole subtree under `PAYMENT_LOCK`. The
//! fold, rather than a sum of the direct children, because money handed back
//! frees the room it took: a charge paid in full and then refunded is owed
//! again, and must be payable again. Every write that *moves* that arithmetic
//! takes the same lock — [`reversal`] included, since a reversal
//! landing between a payment's fold and its append would admit the payment
//! against room it no longer has.
//!
//! The lock is what makes that check mean anything, and it is load-bearing:
//! the fold is a count-then-write across rows, and SurrealDB does not
//! conflict-check a cross-record read against a concurrent insert (the
//! write-skew this repo has hit before), so a fold left unserialized would let
//! two payments both see room and both take it. Running as one process buys
//! nothing on its own — two request tasks interleave across the fold's `await`
//! exactly as two machines would. The cap therefore holds only for as long as
//! *every* write that moves this arithmetic is taken under `PAYMENT_LOCK`;
//! a future append that skips it re-opens the hole silently, which is why the
//! rule is stated here rather than left to be noticed. If an over-payment ever
//! does land it is not a crisis — an over-paid charge is plainly visible in the
//! statement and undone by appending a refund, and both entries are true
//! records of money that really arrived. A CAS counter row was rejected —
//! refunding would have to decrement it, which is a stored derived balance by
//! another name. What the fold costs is also what bounds it: at most
//! [`MAX_LEDGER_APPLIED_LINES`] lines may be applied to any one line, since
//! every one of them is another query taken with the lock held.
//!
//! The `request_key` mismatch `409` rides on the same lock: it is a
//! read-then-compare, and it is read *inside* the lock, so two first-time posts
//! of one key with different amounts cannot both walk past it. One line, one
//! amount, no double charge, and the "you reused a key" diagnostic is exact.

use tokio::sync::Mutex;

use crate::constant::MAX_LEDGER_APPLIED_LINES;
use crate::database::Database;
use crate::db::payment_ledger;
use crate::domain::payment_ledger::{
    LedgerAmount, LedgerMethod, LedgerNote, PaymentLedger, PaymentLedgerId, PaymentLedgerKind,
    PaymentRequestKey,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Serializes the over-payment check, the append it authorizes, and every
/// other append that changes the arithmetic that check does — nothing else.
/// It is the whole guarantee behind that cap, not a convenience: the fold it
/// protects is a cross-record read the database will not conflict-check
/// against a concurrent insert. Held across no other lock, and no other lock is
/// taken while it is held.
pub(crate) static PAYMENT_LOCK: Mutex<()> = Mutex::const_new(());

/// Money in, against one named `charge`. Partial payments are the norm, so
/// a charge may collect several credits; together they may not exceed it
/// (advisory — see the module doc's accepted race). A *reversed* charge
/// takes no payment either, and is refused saying so rather than claiming
/// it was paid.
///
/// With a `request_key` the line is keyed by it (see
/// [`PaymentLedgerId::for_request`]) and the call is retry-safe; without
/// one the id is a fresh ulid and two identical calls are two payments,
/// which is what a cash desk taking the same amount twice really means.
pub async fn credit(
    db: &Database,
    charge: &PaymentLedger,
    amount_minor: LedgerAmount,
    method: Option<LedgerMethod>,
    note: Option<LedgerNote>,
    request_key: Option<&PaymentRequestKey>,
    recorded_by: &UserId,
) -> Result<PaymentLedger, AppError> {
    against(
        db,
        charge,
        PaymentLedgerKind::Charge,
        PaymentLedgerKind::Credit,
        "a payment must be recorded against a charge",
        "the charge is already paid in full",
        // A charge *is* reversible, and its reversal fills the same room a
        // payment would, so the fold refuses the two cases identically.
        Some("the charge was reversed, so it is no longer owed"),
        amount_minor,
        method,
        note,
        request_key.map(|key| PaymentLedgerId::for_request(charge.get_id(), "k", key)),
        recorded_by,
    )
    .await
}

/// Money back out, against one named `credit` — how an over-payment or a
/// payment recorded in error is returned. Partials allowed, and no more
/// than the credit was worth (advisory, same race). `request_key` makes it
/// retry-safe exactly as it does for [`credit`].
pub async fn refund(
    db: &Database,
    credit: &PaymentLedger,
    amount_minor: LedgerAmount,
    method: Option<LedgerMethod>,
    note: Option<LedgerNote>,
    request_key: Option<&PaymentRequestKey>,
    recorded_by: &UserId,
) -> Result<PaymentLedger, AppError> {
    against(
        db,
        credit,
        PaymentLedgerKind::Credit,
        PaymentLedgerKind::Refund,
        "a refund must be recorded against a payment",
        "the payment is already refunded in full",
        // A credit is never reversed ([`reversal`] refuses
        // it), so a full fold here can only mean the refunds, and there is
        // no second reason to tell apart — nor a read to spend looking.
        None,
        amount_minor,
        method,
        note,
        request_key.map(|key| PaymentLedgerId::for_request(credit.get_id(), "kr", key)),
        recorded_by,
    )
    .await
}

/// The shared body of `credit` and `refund`: check the target is the kind
/// this line may point at, then append under [`PAYMENT_LOCK`] while the
/// target's existing children still sum below its amount.
///
/// The replay read comes **before** the cap, and inside the lock: on a
/// retry the first attempt's line is already part of the subtree the cap
/// folds, so consulting the cap first would answer a payment that landed
/// with "already paid in full" — refusing precisely the request that
/// succeeded. Reading the id under the lock also keeps two simultaneous
/// retries from both walking into the cap check.
#[allow(clippy::too_many_arguments)]
async fn against(
    db: &Database,
    target: &PaymentLedger,
    expected: PaymentLedgerKind,
    kind: PaymentLedgerKind,
    wrong_target: &'static str,
    over: &'static str,
    reversed: Option<&'static str>,
    amount_minor: LedgerAmount,
    method: Option<LedgerMethod>,
    note: Option<LedgerNote>,
    keyed: Option<PaymentLedgerId>,
    recorded_by: &UserId,
) -> Result<PaymentLedger, AppError> {
    if target.kind != expected {
        return Err(ValidationError::Invalid {
            field: "source",
            reason: wrong_target,
        }
        .into());
    }
    let _guard = PAYMENT_LOCK.lock().await;
    if let Some(id) = &keyed
        && let Some(existing) = payment_ledger::read(db, id).await?
    {
        // A replay is answered from the stored line — but only if it is the
        // same money. The same key for a different amount or a different
        // target is a client bug, and handing back the old line would hide
        // it behind a `201`.
        if existing.amount_minor.as_minor() != amount_minor.as_minor()
            || existing.get_source_key() != Some(target.id.key())
        {
            return Err(AppError::Conflict(
                "this request_key was already used for a different amount or target",
            ));
        }
        return Ok(existing);
    }
    let taken = applied_to(db, target).await?;
    if taken.saturating_add(amount_minor.as_minor()) > target.amount_minor.as_minor() {
        // The fold cannot say *why* the room is gone: a reversal is a child
        // of the line it undoes and folds in at `+amount` exactly as a
        // payment does, so a reversed charge reads as full to the kuruş.
        // The stored answer is the same either way — nothing may be
        // recorded against it — but "already paid in full" told a bursar
        // money had arrived when none ever did. The reversal's id is
        // derived from its target's, so telling the two apart is one read,
        // taken only on the refusal path.
        if let Some(reversed) = reversed
            && payment_ledger::read(db, &PaymentLedgerId::for_reversal(&target.id))
                .await?
                .is_some()
        {
            return Err(AppError::Conflict(reversed));
        }
        return Err(AppError::Conflict(over));
    }
    payment_ledger::append(
        db,
        PaymentLedger {
            id: keyed.unwrap_or_else(PaymentLedgerId::generate),
            student: target.student.clone(),
            kind,
            amount_minor,
            source: Some(target.id.record()),
            due_at: None,
            method,
            note,
            recorded_by: recorded_by.clone(),
            created_at: Timestamp::now(),
        },
    )
    .await
}

/// How much of `target` is already taken up — the balance fold restricted
/// to everything that points at it, however deep, and read the way `target`
/// is measured.
///
/// The whole subtree, not just the direct children, because money given
/// back frees the room it took: a charge paid in full and then *refunded*
/// is owed again, so it must be payable again (the fold nets to zero). The
/// same walk answers both sides, because the signs already say what each
/// line does — a credit adds under a charge, its refund takes that back,
/// and a reversal of that refund puts it back once more. `-target.sign()`
/// is what flips the reading for a credit, whose room is measured in the
/// refunds against it.
///
/// **The subtree is bounded, and that is what makes the walk affordable.**
/// One query per line, all of them while [`PAYMENT_LOCK`] is held, so an
/// unbounded subtree stalls every other payment in the school: a charge
/// settled 2 000 kuruş at a time would make the next payment issue 2 001
/// sequential queries under the lock. Past
/// [`MAX_LEDGER_APPLIED_LINES`] the walk stops where it is and the write is
/// refused — the count is what is refused, never the money already
/// recorded, so a line that is *already* over the ceiling (written before
/// it existed) still reads, still refunds through its own children, and is
/// still reversible. Only a fresh line applied to *it* is turned away.
// corner-cut: one query per line, ceiling MAX_LEDGER_APPLIED_LINES; fold the
// walk into one recursive statement if that ceiling ever has to rise.
async fn applied_to(db: &Database, target: &PaymentLedger) -> Result<i64, AppError> {
    let mut total = 0i64;
    let mut seen = 0usize;
    // The graph is a DAG by construction — a line can only point at one
    // that already existed — so the walk terminates.
    let mut pending = vec![target.id.clone()];
    while let Some(id) = pending.pop() {
        for child in payment_ledger::list_for_source(db, &id).await? {
            seen += 1;
            if seen >= MAX_LEDGER_APPLIED_LINES {
                return Err(AppError::Conflict(
                    "this line already carries the most lines that may be applied to it; \
                     record the rest against another",
                ));
            }
            total = total.saturating_add(child.kind.sign() * child.amount_minor.as_minor());
            pending.push(child.id);
        }
    }
    Ok(total * -target.kind.sign())
}

/// Undo a line entered by mistake, for its exact amount, with `source`
/// pointing at it. The line itself is never touched.
///
/// Only a *negative-fold* line may be reversed: a charge that should never
/// have been raised, or a refund that should never have been paid out. A
/// mistaken credit is corrected by a [`refund`] against it,
/// which is the same money leaving the school and is spelled that one way.
/// Keyed `<line>_r`, so a retried undo reverses once.
///
/// Appends under `PAYMENT_LOCK` — not for its own sake (the id makes it
/// idempotent by itself) but for the cap's: a reversal *changes* the
/// subtree `applied_to` folds, so one landing between a
/// concurrent payment's fold and its append would let that payment be
/// admitted against room it no longer has. The lock is what makes the cap
/// exact, which is the promise this module's doc makes.
pub async fn reversal(
    db: &Database,
    line: &PaymentLedger,
    note: Option<LedgerNote>,
    recorded_by: &UserId,
) -> Result<PaymentLedger, AppError> {
    if !matches!(
        line.kind,
        PaymentLedgerKind::Charge | PaymentLedgerKind::Refund
    ) {
        return Err(ValidationError::Invalid {
            field: "source",
            reason: "only a charge or a refund can be reversed; refund a payment instead",
        }
        .into());
    }
    let _guard = PAYMENT_LOCK.lock().await;
    payment_ledger::append(
        db,
        PaymentLedger {
            id: PaymentLedgerId::for_reversal(line.get_id()),
            student: line.student.clone(),
            kind: PaymentLedgerKind::Reversal,
            amount_minor: line.amount_minor,
            source: Some(line.id.record()),
            due_at: None,
            method: None,
            note,
            recorded_by: recorded_by.clone(),
            created_at: Timestamp::now(),
        },
    )
    .await
}

pub async fn read(db: &Database, id: &PaymentLedgerId) -> Result<Option<PaymentLedger>, AppError> {
    payment_ledger::read(db, id).await
}

/// A student's whole statement, newest first.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<PaymentLedger>, i64), AppError> {
    payment_ledger::list_for_student(db, student, limit, offset).await
}

/// The derived balance: `credits + reversals - charges - refunds`, in minor
/// units. Negative means the family owes the school.
pub async fn balance_of(db: &Database, student: &UserId) -> Result<i64, AppError> {
    payment_ledger::balance_of(db, student).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raise one charge of `amount` on a fresh student, and hand it back.
    async fn one_charge(amount: i64, db: &Database) -> (PaymentLedger, UserId, UserId) {
        use crate::domain::fee_plan::{FeePlan, FeePlanName, Installment};
        use crate::domain::fee_plan_assignment::FeePlanAssignment;

        let manager = UserId::from_key("mgr1");
        let student = UserId::from_key("stu1");
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
            vec![Installment::new(
                LedgerAmount::try_new(amount).unwrap(),
                Timestamp::from_millis(1_000),
            )],
            &manager,
            db,
        )
        .await
        .unwrap();
        FeePlanAssignment::assign(&plan, &student, &manager, db)
            .await
            .unwrap();
        let (lines, _) = payment_ledger::list_for_student(db, &student, None, 0)
            .await
            .unwrap();
        (lines.into_iter().next().unwrap(), student, manager)
    }

    /// The collision the id grammar must not admit: with `_` legal in a key, a
    /// refund keyed `abc_r` derived exactly `<refund>_r` — the id that refund's
    /// own reversal must own — and whichever came second was handed the other's
    /// line, of the wrong kind and the wrong amount, with the real reversal
    /// impossible ever after. `_` is now illegal in a key, so the collision
    /// cannot be constructed; the nearest legal key still works, and the
    /// reversal still gets its own line.
    #[tokio::test]
    async fn a_keyed_refund_cannot_take_the_id_of_its_own_reversal() {
        assert!(
            PaymentRequestKey::try_new("abc_r").is_err(),
            "the key that spelled a reversal's id must not parse"
        );
        let db = crate::database::init_mem().await.unwrap();
        let (charge, _, manager) = one_charge(100, &db).await;
        let key = PaymentRequestKey::try_new("abc-r").unwrap();
        let credit_line = credit(
            &db,
            &charge,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
        )
        .await
        .unwrap();
        let refund_line = refund(
            &db,
            &credit_line,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            Some(&key),
            &manager,
        )
        .await
        .unwrap();
        let reversal_line = reversal(&db, &refund_line, None, &manager).await.unwrap();
        assert_ne!(reversal_line.get_id(), refund_line.get_id());
        assert_eq!(reversal_line.get_kind(), PaymentLedgerKind::Reversal);
        assert_eq!(reversal_line.get_amount_minor().as_minor(), 100);

        // And the last-ditch guard bites: an id that resolves to another kind
        // is a 500, never that other line handed back as this one.
        let intruder = PaymentLedger {
            id: charge.get_id().clone(),
            student: charge.student.clone(),
            kind: PaymentLedgerKind::Credit,
            amount_minor: LedgerAmount::try_new(1).unwrap(),
            source: Some(charge.id.record()),
            due_at: None,
            method: None,
            note: None,
            recorded_by: manager.clone(),
            created_at: Timestamp::now(),
        };
        assert!(
            matches!(
                payment_ledger::append(&db, intruder).await,
                Err(AppError::Internal(_))
            ),
            "a read-back of another kind must not pass for the appended line"
        );
    }

    /// The defect this fold replaced a plain sum for: a payment that was handed
    /// back frees the room it took, so the charge it paid can be paid again. It
    /// is the same charge, still owed — refusing the second payment would leave
    /// a family unable to settle a bill the school itself refunded.
    #[tokio::test]
    async fn a_refund_frees_the_room_it_took_under_the_charge() {
        let db = crate::database::init_mem().await.unwrap();
        let (charge, student, manager) = one_charge(100, &db).await;
        let pay = |amount| {
            credit(
                &db,
                &charge,
                LedgerAmount::try_new(amount).unwrap(),
                None,
                None,
                None,
                &manager,
            )
        };

        let credit_line = pay(100).await.unwrap();
        // Still fully paid: a second payment has no room.
        assert!(
            matches!(pay(100).await, Err(AppError::Conflict(_))),
            "a charge paid in full may not be paid twice"
        );

        refund(
            &db,
            &credit_line,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
        )
        .await
        .unwrap();
        assert_eq!(
            balance_of(&db, &student).await.unwrap(),
            -100,
            "the refund puts the charge back on the family"
        );
        let repaid = pay(100)
            .await
            .expect("the refunded charge is payable again");
        assert_eq!(repaid.get_amount_minor().as_minor(), 100);
        assert_eq!(balance_of(&db, &student).await.unwrap(), 0);

        // And the room is gone again, so the cap still bites after all that.
        assert!(matches!(pay(1).await, Err(AppError::Conflict(_))));
    }

    /// The other half of the fold: reversing a refund un-does the money going
    /// out, so the room that refund freed is taken back.
    #[tokio::test]
    async fn a_reversed_refund_takes_its_room_back() {
        let db = crate::database::init_mem().await.unwrap();
        let (charge, _, manager) = one_charge(100, &db).await;
        let credit_line = credit(
            &db,
            &charge,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
        )
        .await
        .unwrap();
        let refund_line = refund(
            &db,
            &credit_line,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
        )
        .await
        .unwrap();
        reversal(&db, &refund_line, None, &manager).await.unwrap();

        assert!(
            matches!(
                credit(
                    &db,
                    &charge,
                    LedgerAmount::try_new(100).unwrap(),
                    None,
                    None,
                    None,
                    &manager
                )
                .await,
                Err(AppError::Conflict(_))
            ),
            "the refund was undone, so the charge is paid in full again"
        );
    }

    /// The money branches, end to end against the engine: partial payments
    /// accumulate, the advisory cap refuses the one that would overshoot the
    /// charge, a refund is capped by *its* credit, and neither a credit nor a
    /// reversal may point at a line of the wrong kind.
    #[tokio::test]
    async fn payments_are_capped_by_the_line_they_target() {
        use crate::domain::fee_plan::{FeePlan, FeePlanName, Installment};
        use crate::domain::fee_plan_assignment::FeePlanAssignment;

        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("mgr1");
        let student = UserId::from_key("stu1");
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
            vec![Installment::new(
                LedgerAmount::try_new(10_000).unwrap(),
                Timestamp::from_millis(1_000),
            )],
            &manager,
            &db,
        )
        .await
        .unwrap();
        FeePlanAssignment::assign(&plan, &student, &manager, &db)
            .await
            .unwrap();
        let (charges, _) = payment_ledger::list_for_student(&db, &student, None, 0)
            .await
            .unwrap();
        let charge = charges.into_iter().next().unwrap();
        assert_eq!(charge.get_due_at(), Some(Timestamp::from_millis(1_000)));

        let paid = |amount| {
            credit(
                &db,
                &charge,
                LedgerAmount::try_new(amount).unwrap(),
                None,
                None,
                None,
                &manager,
            )
        };
        let first = paid(6_000).await.unwrap();
        assert!(
            matches!(paid(5_000).await, Err(AppError::Conflict(_))),
            "6 000 + 5 000 overshoots a 10 000 charge"
        );
        paid(4_000).await.expect("the exact remainder is allowed");
        assert_eq!(balance_of(&db, &student).await.unwrap(), 0);

        // A refund is bounded by the credit it returns, not by the charge.
        let refund_amount = |amount| {
            refund(
                &db,
                &first,
                LedgerAmount::try_new(amount).unwrap(),
                None,
                None,
                None,
                &manager,
            )
        };
        assert!(matches!(
            refund_amount(6_001).await,
            Err(AppError::Conflict(_))
        ));
        refund_amount(6_000).await.unwrap();
        assert_eq!(
            balance_of(&db, &student).await.unwrap(),
            -6_000,
            "money handed back puts the debt back on the family"
        );

        // Wrong-kind targets are refused, and a credit is never reversed.
        assert!(
            credit(
                &db,
                &first,
                LedgerAmount::try_new(1).unwrap(),
                None,
                None,
                None,
                &manager,
            )
            .await
            .is_err()
        );
        assert!(reversal(&db, &first, None, &manager).await.is_err());
        // A charge, though, reverses — once, however often it is retried.
        let reversed = reversal(&db, &charge, None, &manager).await.unwrap();
        let again = reversal(&db, &charge, None, &manager).await.unwrap();
        assert_eq!(reversed.get_id(), again.get_id());
        let (lines, _) = payment_ledger::list_for_student(&db, &student, None, 0)
            .await
            .unwrap();
        assert_eq!(lines.len(), 5, "charge, two credits, refund, one reversal");
    }
}
