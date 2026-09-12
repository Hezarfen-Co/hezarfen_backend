//! The `payment_ledger` table: the append-only `INSERT`s and the reads the
//! balance and the over-payment cap fold. Nothing here ever `UPDATE`s or
//! `DELETE`s a row — a mistake is corrected by appending the opposing line.
//! The row locks that serialize the money arithmetic around these calls are
//! taken by the workflows in [`crate::service::payment_ledger`]
//! ([`lock_for_cap`]).
//!
//! The ledger row's `id` is a derived TEXT primary key, so Postgres itself is
//! the uniqueness check: a duplicate insert of the same key is the
//! "already billed" answer, surfaced here as
//! `INSERT … ON CONFLICT (id) DO NOTHING` plus a read-back of the row that
//! won. That is what makes every deterministic id (see
//! [`PaymentLedgerId::for_installment`] and friends) an idempotence key —
//! replaying the same append is a no-op, never a second line and never an
//! edit of the first, however the concurrent writers interleave.

use sqlx::PgExecutor;

use crate::constant::MAX_LEDGER_APPLIED_LINES;
use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::fee_plan::Installment;
use crate::domain::fee_plan_assignment::FeePlanAssignment;
use crate::domain::payment_ledger::{
    LedgerAmount, LedgerMethod, LedgerNote, PaymentLedger, PaymentLedgerId, PaymentLedgerKind,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The only writer: one `INSERT`, no update path anywhere in this module.
///
/// A line whose id already exists is left exactly as it is — the row wins,
/// the write is dropped, and the winner's line is read back in the *same
/// statement* (`ON CONFLICT (id) DO NOTHING`, and the `UNION ALL` arm answers
/// with the stored row when the insert was skipped). Unlike a scan, the point
/// read on the id cannot miss a row a concurrent writer just committed: an
/// insert that races an identical id waits on the unique index until the
/// other transaction settles, then takes the read-back arm.
///
/// The read-back also checks the stored line is the *kind* that was being
/// appended. Handing a caller someone else's row is only safe while every id
/// shape is unambiguous (see [`PaymentLedgerId::for_request`]); if a future
/// marker or a widened key charset ever let two shapes meet, this is the
/// check that turns silently-wrong money into a 500 instead. It should be
/// unreachable, and it is cheap enough to keep it that way.
pub(crate) async fn append(
    exe: impl PgExecutor<'_>,
    row: PaymentLedger,
) -> Result<PaymentLedger, AppError> {
    let created = sqlx::query_as!(
        PaymentLedger,
        r#"WITH ins AS (
               INSERT INTO payment_ledger
                   (id, student, kind, amount_minor, source, due_at, method, note,
                    recorded_by, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
               ON CONFLICT (id) DO NOTHING
               RETURNING id, student, kind, amount_minor, source, due_at, method, note,
                         recorded_by, created_at)
           SELECT id AS "id!: PaymentLedgerId", student AS "student!: UserId",
                  kind AS "kind!: PaymentLedgerKind",
                  amount_minor AS "amount_minor!: LedgerAmount", source,
                  due_at AS "due_at: Timestamp", method AS "method: LedgerMethod",
                  note AS "note: LedgerNote",
                  recorded_by AS "recorded_by!: UserId", created_at AS "created_at!: Timestamp"
           FROM ins
           UNION ALL
           SELECT id AS "id!: PaymentLedgerId", student AS "student!: UserId",
                  kind AS "kind!: PaymentLedgerKind",
                  amount_minor AS "amount_minor!: LedgerAmount", source,
                  due_at AS "due_at: Timestamp", method AS "method: LedgerMethod",
                  note AS "note: LedgerNote",
                  recorded_by AS "recorded_by!: UserId", created_at AS "created_at!: Timestamp"
           FROM payment_ledger
           WHERE id = $1 AND NOT EXISTS (SELECT 1 FROM ins)"#,
        row.id.key(),
        row.student.uuid(),
        row.kind.as_str(),
        row.amount_minor.as_minor(),
        row.source,
        row.due_at.map(|t| t.as_millis()),
        row.method.as_ref().map(|m| m.as_str()),
        row.note.as_ref().map(|n| n.as_str()),
        row.recorded_by.uuid(),
        row.created_at.as_millis(),
    )
    .fetch_optional(exe)
    .await?;
    match created {
        Some(line) if line.kind == row.kind => Ok(line),
        Some(_) => Err(AppError::Internal(
            "a ledger id resolved to a line of another kind".into(),
        )),
        // Unreachable: the statement either inserts or reads back the row
        // with that very id.
        None => Err(AppError::Internal("failed to write the ledger line".into())),
    }
}

pub async fn read(
    exe: impl PgExecutor<'_>,
    id: &PaymentLedgerId,
) -> Result<Option<PaymentLedger>, AppError> {
    sqlx::query_as!(
        PaymentLedger,
        "SELECT id AS \"id: PaymentLedgerId\", student AS \"student: UserId\",
                kind AS \"kind: PaymentLedgerKind\", amount_minor AS \"amount_minor: LedgerAmount\",
                source, due_at AS \"due_at: Timestamp\", method AS \"method: LedgerMethod\",
                note AS \"note: LedgerNote\", recorded_by AS \"recorded_by: UserId\",
                created_at AS \"created_at: Timestamp\"
         FROM payment_ledger WHERE id = $1",
        id.key(),
    )
    .fetch_optional(exe)
    .await
    .map_err(Into::into)
}

/// Bill installment `n` (1-based) of `assignment`, at the amount and due
/// date the plan carried when it was assigned — the charge is a frozen
/// copy, so editing the plan afterwards moves no existing line.
///
/// Replaying it is free: the id is `(plan, student, n)`, so a duplicate
/// assign writes nothing and an assignment whose charges landed only in
/// part self-heals on the next assign of the same pair.
pub async fn charge_for_installment(
    exe: impl PgExecutor<'_>,
    assignment: &FeePlanAssignment,
    n: usize,
    installment: &Installment,
    recorded_by: &UserId,
) -> Result<PaymentLedger, AppError> {
    append(
        exe,
        PaymentLedger {
            id: PaymentLedgerId::for_installment(&assignment.get_id(), n),
            student: assignment.get_student().clone(),
            kind: PaymentLedgerKind::Charge,
            amount_minor: installment.get_amount_minor(),
            source: Some(assignment.get_id().key()),
            due_at: Some(installment.get_due_at()),
            method: None,
            note: None,
            recorded_by: recorded_by.clone(),
            created_at: Timestamp::now(),
        },
    )
    .await
}

/// A student's whole statement, newest first.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<PaymentLedger>, i64), AppError> {
    PagedList::new(
        "payment_ledger WHERE student = $1",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind(student.uuid())
    .run(limit, offset, db)
    .await
}

/// One kind's fold, as [`balance_of`] and [`applied_to`] read it off the
/// `GROUP BY` — at most four rows, never the lines behind them.
struct KindFold {
    kind: PaymentLedgerKind,
    total: i64,
    /// Lines behind the fold. [`balance_of`] never looks at it; [`applied_to`]
    /// enforces the applied-lines ceiling with it.
    lines: i64,
}

/// The derived balance: `credits + reversals - charges - refunds`, in minor
/// units. Negative means the family owes the school. Never stored anywhere.
///
/// **The sum is taken per kind by the database** and only the four totals
/// come back, so a balance read costs the same whether the ledger holds
/// four lines or forty thousand.
///
/// **The signs stay here**, applied by the same
/// `PaymentLedgerKind::sign` the documented formula is spelled in —
/// [`PaymentLedger::fold_balance`](crate::domain::payment_ledger::PaymentLedger::fold_balance)
/// folds the grouped totals and the raw lines alike. Summing
/// `IF kind = 'charge' THEN -amount …` in SQL would have folded the whole
/// balance in one statement and forked the one rule that decides what money
/// means into a second language, where nothing fails the day the two
/// disagree. Grouping keeps the aggregate ignorant of signs. A stored
/// running total was the third option and is a counter that can drift — a
/// bug class this repo closes, not one it opens.
pub async fn balance_of(db: &Database, student: &UserId) -> Result<i64, AppError> {
    let totals = sqlx::query_as!(
        KindFold,
        "SELECT kind AS \"kind!: PaymentLedgerKind\", \
                COALESCE(sum(amount_minor), 0)::bigint AS \"total!: i64\", \
                count(*)::bigint AS \"lines!: i64\" \
         FROM payment_ledger WHERE student = $1 GROUP BY kind",
        student.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(PaymentLedger::fold_balance(
        totals.into_iter().map(|row| (row.kind, row.total)),
    ))
}

/// Lock the **ancestor chain** of `id` (`FOR UPDATE`, the line first, then
/// its target, and so on up to a charge). Every write that moves the money
/// arithmetic of a subtree takes this walk in its transaction, so writers to
/// one subtree all contend on its **root's** row lock: a payment folding a
/// charge's subtree cannot land between a refund's fold and its append
/// deeper down. That is the whole guarantee the over-payment cap rests on —
/// it replaces a process-wide mutex with the rows the money lives on.
///
/// Lock order is *deepest first*: a writer always wants the next ancestor,
/// never the reverse, and an ancestor is never a descendant, so no two
/// walks can cycle — no deadlock, nothing for the retry loop to unwind.
/// The chain is at most three rows (charge ← credit ← refund; a reversal
/// points at a charge or a refund), and a charge's `source` names a
/// fee-plan assignment rather than a ledger row, which is where the walk
/// stops.
pub(crate) async fn lock_for_cap(
    exe: &mut sqlx::PgConnection,
    id: &PaymentLedgerId,
) -> Result<(), AppError> {
    let mut next = Some(id.clone());
    while let Some(key) = next {
        let row = sqlx::query!(
            r#"SELECT kind AS "kind: PaymentLedgerKind", source
               FROM payment_ledger WHERE id = $1 FOR UPDATE"#,
            key.key(),
        )
        .fetch_optional(&mut *exe)
        .await?;
        next = match row {
            // Keep walking while the row points at another *ledger* row.
            Some(row) if row.kind != PaymentLedgerKind::Charge => {
                row.source.map(|s| PaymentLedgerId::from_key(&s))
            }
            // A charge's source is an assignment key, not a ledger row —
            // and by then the root lock is held, which is the point.
            _ => None,
        };
    }
    Ok(())
}

/// How much of `target` is already taken up — the balance fold restricted
/// to everything that points at it, however deep, and read the way `target`
/// is measured. Caller holds [`lock_for_cap`] on `target`'s chain in the
/// same transaction, so the fold and the append it authorizes cannot be
/// separated by a competing write.
///
/// The whole subtree, not just the direct children, because money given
/// back frees the room it took: a charge paid in full and then *refunded*
/// is owed again, so it must be payable again (the fold nets to zero). The
/// same walk answers both sides, because the signs already say what each
/// line does — a credit adds under a charge, its refund takes that back,
/// and a reversal of that refund puts it back once more. `-target.sign()`
/// is what flips the reading for a credit, whose room is measured in the
/// refunds against it. The recursive CTE replaces the old
/// one-query-per-child walk; the signs still fold in Rust, by
/// [`PaymentLedger::fold_balance`](crate::domain::payment_ledger::PaymentLedger::fold_balance).
///
/// **The ceiling stays.** Past [`MAX_LEDGER_APPLIED_LINES`] lines the fold
/// refuses — the count is what is refused, never the money already
/// recorded, so a line that is *already* over the ceiling still reads,
/// still refunds through its own children, and is still reversible. Only a
/// fresh line applied to *it* is turned away.
pub(crate) async fn applied_to(
    exe: &mut sqlx::PgConnection,
    target_key: &str,
    target_kind: PaymentLedgerKind,
) -> Result<i64, AppError> {
    let folds = sqlx::query_as!(
        KindFold,
        r#"WITH RECURSIVE applied(id, kind, amount_minor) AS (
               SELECT id, kind, amount_minor FROM payment_ledger WHERE source = $1
               UNION ALL
               SELECT l.id, l.kind, l.amount_minor
               FROM payment_ledger l JOIN applied a ON l.source = a.id
           )
           SELECT kind AS "kind!: PaymentLedgerKind",
                  COALESCE(sum(amount_minor), 0)::bigint AS "total!: i64",
                  count(*)::bigint AS "lines!: i64"
           FROM applied GROUP BY kind"#,
        target_key,
    )
    .fetch_all(exe)
    .await?;
    let lines: i64 = folds.iter().map(|fold| fold.lines).sum();
    if lines as usize >= MAX_LEDGER_APPLIED_LINES {
        return Err(AppError::Conflict(
            "this line already carries the most lines that may be applied to it; \
             record the rest against another",
        ));
    }
    let total = PaymentLedger::fold_balance(folds.into_iter().map(|fold| (fold.kind, fold.total)));
    Ok(total * -target_kind.sign())
}

