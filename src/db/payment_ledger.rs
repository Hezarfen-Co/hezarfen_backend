//! The `payment_ledger` table: the append-only `CREATE`s and the reads the
//! balance and the statement fold. Nothing here ever `UPDATE`s or `DELETE`s a
//! row — a mistake is corrected by appending the opposing line. The lock that
//! serializes the money arithmetic around these calls lives in
//! [`crate::service::payment_ledger`].

use surrealdb::types::{AlreadyExistsError, SurrealValue};

use crate::constant::{CAS_UPDATE_RETRIES, PAYMENT_LEDGER_TABLE};
use crate::database::{Database, lost_the_race};
use crate::db::page::PagedList;
use crate::domain::fee_plan::Installment;
use crate::domain::fee_plan_assignment::FeePlanAssignment;
use crate::domain::payment_ledger::{PaymentLedger, PaymentLedgerId, PaymentLedgerKind};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Did this `CREATE` fail *only* because the row is already there? Matched on
/// the SDK's typed `AlreadyExists`/`Record` detail — never on the message text
/// and never on "any database error", because swallowing a real fault in money
/// code would be far worse than the 500 it saves.
fn is_duplicate_record(error: &surrealdb::Error) -> bool {
    matches!(
        error.already_exists_details(),
        Some(AlreadyExistsError::Record { .. })
    )
}

/// The only writer: one `CREATE`, no update path anywhere in this module.
///
/// A line whose id already exists is left exactly as it is — the row wins,
/// the write is dropped. That is what makes a deterministic id (see
/// [`PaymentLedgerId::for_installment`]) an idempotence key: replaying the
/// same append is a no-op, never a second line and never an edit of the
/// first. The point read is on the id itself, so unlike a scan it cannot
/// miss a row a concurrent writer just made — but it is only a fast path.
/// The guarantee is `CREATE`'s own: on an existing id it *errors* and
/// leaves the row untouched, so the writer that lost the race reads back
/// the winner's line instead of failing the request with a 500.
///
/// A *write conflict* is the same race decided one layer down — two appends
/// of one id arriving together are no longer serialized by a process-wide
/// lock, so the store aborts one as retryable instead of answering it
/// "already exists". Both are read back the same way, and a conflict that
/// turns out to have written nothing is simply tried again; no path here
/// can write a second line, since the id is the key.
///
/// Both read-backs check the stored line is the *kind* that was being
/// appended. Handing a caller someone else's row is only safe while every
/// id shape is unambiguous (see [`PaymentLedgerId::for_request`]); if a
/// future marker or a widened key charset ever let two shapes meet, this is
/// the check that turns silently-wrong money into a 500 instead. It should
/// be unreachable, and it is cheap enough to keep it that way.
pub(crate) async fn append(db: &Database, row: PaymentLedger) -> Result<PaymentLedger, AppError> {
    let same_kind = |existing: PaymentLedger| {
        if existing.kind == row.kind {
            Ok(existing)
        } else {
            Err(AppError::Internal(
                "a ledger id resolved to a line of another kind".into(),
            ))
        }
    };
    if let Some(existing) = read(db, &row.id).await? {
        return same_kind(existing);
    }
    let id = row.id.clone();
    for _ in 0..CAS_UPDATE_RETRIES {
        match db.create(id.record()).content(row.clone()).await {
            Ok(Some(created)) => return Ok(created),
            Ok(None) => break,
            Err(e) if is_duplicate_record(&e) || lost_the_race(&e) => {
                if let Some(existing) = read(db, &id).await? {
                    return same_kind(existing);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal("failed to write the ledger line".into()))
}

pub async fn read(db: &Database, id: &PaymentLedgerId) -> Result<Option<PaymentLedger>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Bill installment `n` (1-based) of `assignment`, at the amount and due
/// date the plan carried when it was assigned — the charge is a frozen
/// copy, so editing the plan afterwards moves no existing line.
///
/// Replaying it is free: the id is `(plan, student, n)`, so a duplicate
/// assign writes nothing and an assignment whose charges landed only in
/// part self-heals on the next assign of the same pair.
pub async fn charge_for_installment(
    db: &Database,
    assignment: &FeePlanAssignment,
    n: usize,
    installment: &Installment,
    recorded_by: &UserId,
) -> Result<PaymentLedger, AppError> {
    append(
        db,
        PaymentLedger {
            id: PaymentLedgerId::for_installment(assignment.get_id(), n),
            student: assignment.get_student().clone(),
            kind: PaymentLedgerKind::Charge,
            amount_minor: installment.get_amount_minor(),
            source: Some(assignment.get_id().record()),
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
        "payment_ledger WHERE student = $student",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind("student", student.record())
    .run(limit, offset, db)
    .await
}

/// Every line pointing at `source`: a charge's payments (and its reversal),
/// a credit's refunds. Feeds the over-payment cap and, later, the statement
/// rollup. Oldest first — this is a history, not a page.
pub async fn list_for_source(
    db: &Database,
    source: &PaymentLedgerId,
) -> Result<Vec<PaymentLedger>, AppError> {
    let mut result = db
        .query("SELECT * FROM payment_ledger WHERE source = $source ORDER BY created_at, id")
        .bind(("source", source.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<PaymentLedger>>(0)?)
}

/// One kind's whole sum, as the `GROUP BY` in [`balance_of`]
/// hands it back — at most four rows, never the lines behind them.
#[derive(Debug, SurrealValue)]
struct KindTotal {
    kind: PaymentLedgerKind,
    total: i64,
}

/// The derived balance: `credits + reversals - charges - refunds`, in minor
/// units. Negative means the family owes the school. Never stored anywhere.
///
/// **The sum is taken per kind by the database** and only the four totals
/// come back, so a balance read costs the same whether the ledger holds
/// four lines or forty thousand. It used to decode and fold every row, and
/// a fee ledger grows by design rather than only under abuse: one
/// assignment appends a charge *per installment* per student (up to
/// [`MAX_FEE_PLAN_ASSIGN_WRITES`](crate::constant::MAX_FEE_PLAN_ASSIGN_WRITES)
/// in a single request), and every one of those rows was decoded again by
/// every later `GET /payments/balance/*`.
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
    let totals: Vec<KindTotal> = db
        .query(format!(
            "SELECT kind, math::sum(amount_minor) AS total \
             FROM {PAYMENT_LEDGER_TABLE} WHERE student = $student GROUP BY kind"
        ))
        .bind(("student", student.record()))
        .await?
        .check()?
        .take(0)?;
    Ok(PaymentLedger::fold_balance(
        totals.into_iter().map(|row| (row.kind, row.total)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::user::UserId;

    /// The mem engine is not the store this runs against, and an aggregate is
    /// exactly where the two have diverged before: `count()` over an *indexed*
    /// field compared to a plan-time value comes back `{count: N}` rather than
    /// a plain int, and `student` is indexed on this table
    /// (`payment_ledger_student`). So the `GROUP BY` is proved on a real
    /// server: that it decodes into [`KindTotal`], that it folds to the
    /// documented figure per kind, that it is scoped to one student, and that
    /// someone with no lines at all comes back `0` rather than an error.
    #[tokio::test]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn the_balance_aggregate_decodes_on_a_real_server() {
        let (db, _serialized) = crate::database::init_test_server("payment_balance_sum").await;
        db.query(
            "CREATE payment_ledger:a SET student = user:ali, kind = 'charge', \
                 amount_minor = 10000, recorded_by = user:adm, created_at = 1;
             CREATE payment_ledger:b SET student = user:ali, kind = 'credit', \
                 amount_minor = 6000, recorded_by = user:adm, created_at = 2;
             CREATE payment_ledger:c SET student = user:ali, kind = 'refund', \
                 amount_minor = 1000, recorded_by = user:adm, created_at = 3;
             CREATE payment_ledger:d SET student = user:ali, kind = 'reversal', \
                 amount_minor = 4500, recorded_by = user:adm, created_at = 4;
             CREATE payment_ledger:e SET student = user:veli, kind = 'credit', \
                 amount_minor = 777, recorded_by = user:adm, created_at = 5;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        let ali = UserId::from_key("ali");
        assert_eq!(
            balance_of(&db, &ali).await.unwrap(),
            6_000 + 4_500 - 10_000 - 1_000,
            "credits + reversals - charges - refunds, and veli's line is not ali's"
        );
        // Somebody with no lines at all: no groups come back, not an error.
        assert_eq!(
            balance_of(&db, &UserId::from_key("nobody")).await.unwrap(),
            0
        );
    }
}
