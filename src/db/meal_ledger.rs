//! The `meal_ledger` table: the append-only money writes and reads. The
//! row/newtype shapes, the deterministic id minters and the sign
//! convention live in [`crate::domain::meal_ledger`]; the charge/reversal
//! and credit workflows that sequence these in [`crate::service::meal_ledger`].

use surrealdb::types::{AlreadyExistsError, SurrealValue};

use crate::constant::{CAS_UPDATE_RETRIES, MEAL_LEDGER_TABLE};
use crate::database::{Database, lost_the_race};
use crate::db::menu_dish;
use crate::db::page::PagedList;
use crate::domain::meal_ledger::{LedgerAmount, MealLedger, MealLedgerId, MealLedgerKind};
use crate::domain::menu::MenuId;
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
/// [`MealLedgerId::for_attempt`]) an idempotence key: replaying the same
/// append is a no-op, never a second line and never an edit of the first.
/// The point read is on the id itself, so unlike a scan it cannot miss a
/// row a concurrent writer just made — but it is only a fast path. The
/// guarantee is `CREATE`'s own: on an existing id it *errors* and leaves
/// the row untouched, so the writer that lost the race reads back the
/// winner's line instead of failing the request with a 500.
///
/// A *write conflict* is the same race decided one layer down — two
/// appends of one id arriving together are no longer serialized by a
/// process-wide lock, so the store aborts one as retryable instead of
/// answering it "already exists". Both are read back the same way, and a
/// conflict that turns out to have written nothing is simply tried again;
/// no path here can write a second line, since the id is the key.
pub(crate) async fn append(db: &Database, row: MealLedger) -> Result<MealLedger, AppError> {
    if let Some(existing) = read(db, &row.id).await? {
        return Ok(existing);
    }
    let id = row.id.clone();
    for _ in 0..CAS_UPDATE_RETRIES {
        match db.create(id.record()).content(row.clone()).await {
            Ok(Some(created)) => return Ok(created),
            Ok(None) => break,
            Err(e) if is_duplicate_record(&e) || lost_the_race(&e) => {
                if let Some(existing) = read(db, &id).await? {
                    return Ok(existing);
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(AppError::Internal("failed to write the ledger line".into()))
}

pub async fn read(db: &Database, id: &MealLedgerId) -> Result<Option<MealLedger>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// What a seat on `menu` costs *right now*: the sum of its dishes. `None`
/// when the menu is free (or empty) — a zero line is noise, not history.
///
/// Called before the seat is taken so an unchargeable menu (one summing
/// past `MAX_LEDGER_AMOUNT_MINOR`) refuses the booking outright instead of
/// leaving a booked-but-unbilled row behind.
pub async fn price_snapshot(
    db: &Database,
    menu: &MenuId,
) -> Result<Option<LedgerAmount>, AppError> {
    let total = menu_dish::list_for_menu(db, menu)
        .await?
        .iter()
        .fold(0i64, |sum, dish| {
            sum.saturating_add(dish.get_price_minor().as_minor())
        });
    if total == 0 {
        return Ok(None);
    }
    Ok(Some(LedgerAmount::try_new(total)?))
}

/// A student's whole statement, newest first.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealLedger>, i64), AppError> {
    PagedList::new(
        "meal_ledger WHERE student = $student",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind("student", student.record())
    .run(limit, offset, db)
    .await
}

/// The derived balance: `credits + reversals - charges`, in minor units.
/// Negative means the student owes the school. Never stored anywhere.
///
/// **The sum is taken per kind by the database** and only the three totals
/// come back, so a balance read costs the same whether the statement holds
/// four lines or forty thousand. It used to decode and fold every line, and
/// nothing bounds a statement's length but the cycle ceiling
/// ([`MAX_MEAL_BOOKING_ATTEMPTS`](crate::constant::MAX_MEAL_BOOKING_ATTEMPTS))
/// added alongside this: every book/cancel
/// pair appends two permanent rows, and each one was paid for again by
/// every later `GET /meals/balance/*`.
///
/// **The signs stay on the kind**, applied by the very
/// [`MealLedgerKind::sign`]
/// the documented formula is spelled in. Summing `IF kind = 'charge' THEN
/// -amount …` in SQL would have folded the whole balance in one statement
/// and forked the one rule that decides what money means into a second
/// language, where nothing would fail the day the two disagreed. Grouping
/// instead keeps the aggregate ignorant of signs: it counts kinds, and
/// Rust still says what a kind does. A stored running total was the third
/// option and is a counter that can drift — a bug class this repo closes,
/// not one it opens.
pub async fn balance_of(db: &Database, student: &UserId) -> Result<i64, AppError> {
    let totals: Vec<KindTotal> = db
        .query(format!(
            "SELECT kind, math::sum(amount_minor) AS total \
             FROM {MEAL_LEDGER_TABLE} WHERE student = $student GROUP BY kind"
        ))
        .bind(("student", student.record()))
        .await?
        .check()?
        .take(0)?;
    Ok(totals
        .iter()
        .fold(0i64, |sum, row| sum + row.kind.sign() * row.total))
}

/// One kind's whole sum, as the `GROUP BY` in [`balance_of`] hands
/// it back — at most three rows, never the lines behind them.
#[derive(Debug, SurrealValue)]
struct KindTotal {
    kind: MealLedgerKind,
    total: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The balance aggregate **against a real server**, and `#[ignore]`d for
    /// it: `init_mem`'s embedded engine is not the store this runs on, and an
    /// aggregate is exactly where the two are known to differ — a `count()`
    /// over an indexed field comes back `{count: N}` from the server and a
    /// bare int from memory, and `student` here *is* indexed. This asserts the
    /// `GROUP BY` really decodes into [`KindTotal`] and folds to the
    /// documented figure, per kind, scoped to one student.
    #[tokio::test]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn the_balance_aggregate_decodes_on_a_real_server() {
        let (db, _serialized) = crate::database::init_test_server("meal_balance_sum").await;
        db.query(
            "CREATE meal_ledger:a SET student = user:ali, kind = 'credit', \
                 amount_minor = 10000, recorded_by = user:adm, created_at = 1;
             CREATE meal_ledger:b SET student = user:ali, kind = 'charge', \
                 amount_minor = 4500, recorded_by = user:adm, created_at = 2;
             CREATE meal_ledger:c SET student = user:ali, kind = 'charge', \
                 amount_minor = 1500, recorded_by = user:adm, created_at = 3;
             CREATE meal_ledger:d SET student = user:ali, kind = 'reversal', \
                 amount_minor = 4500, recorded_by = user:adm, created_at = 4;
             CREATE meal_ledger:e SET student = user:veli, kind = 'credit', \
                 amount_minor = 777, recorded_by = user:adm, created_at = 5;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        let ali = UserId::from_key("ali");
        assert_eq!(
            balance_of(&db, &ali).await.unwrap(),
            10_000 - 4_500 - 1_500 + 4_500
        );
        // Somebody with no lines at all: no groups come back, not an error.
        assert_eq!(
            balance_of(&db, &UserId::from_key("nobody")).await.unwrap(),
            0
        );
    }
}
