//! The `meal_ledger` table: the append-only money writes and reads. The
//! row/newtype shapes, the deterministic id minters and the sign
//! convention live in [`crate::domain::meal_ledger`]; the charge/reversal
//! and credit workflows that sequence these in [`crate::service::meal_ledger`].

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::meal_ledger::{
    LedgerAmount, LedgerMethod, LedgerNote, MealLedger, MealLedgerId, MealLedgerKind,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::menu::MenuId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The only writer: one `INSERT`, no update path anywhere in this module.
///
/// A line whose id already exists is left exactly as it is — the row wins,
/// the write is dropped. That is what makes a deterministic id (see
/// [`MealLedgerId::for_attempt`]) an idempotence key: replaying the same
/// append is a no-op, never a second line and never an edit of the first.
/// The point read is on the id itself, so unlike a scan it cannot miss a
/// row a concurrent writer just made — but it is only a fast path. The
/// guarantee is the insert's own `ON CONFLICT (id) DO NOTHING`: on an
/// existing id nothing is written, so the writer that lost the race reads
/// back the winner's line instead of failing the request with a 500.
pub(crate) async fn append(db: &Database, row: MealLedger) -> Result<MealLedger, AppError> {
    if let Some(existing) = read(db, &row.id).await? {
        return Ok(existing);
    }
    let inserted = sqlx::query_as!(
        MealLedger,
        "INSERT INTO meal_ledger
             (id, student, kind, amount_minor, source, method, note, recorded_by, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (id) DO NOTHING
         RETURNING id AS \"id: MealLedgerId\", student AS \"student: UserId\", kind AS \"kind: MealLedgerKind\", amount_minor AS \"amount_minor: LedgerAmount\", source, method AS \"method: LedgerMethod\", note AS \"note: LedgerNote\", recorded_by AS \"recorded_by: UserId\", created_at AS \"created_at: Timestamp\"",
        row.id.key(),
        row.student.uuid(),
        row.kind.as_str(),
        row.amount_minor.as_minor(),
        row.source,
        row.method.as_ref().map(|m| m.as_str()),
        row.note.as_ref().map(|n| n.as_str()),
        row.recorded_by.uuid(),
        row.created_at.as_millis(),
    )
    .fetch_optional(db)
    .await?;
    match inserted {
        Some(created) => Ok(created),
        // Lost the id to a concurrent append of the same line: the stored
        // row is the answer, never a second line.
        None => read(db, &row.id)
            .await?
            .ok_or_else(|| AppError::Internal("failed to write the ledger line".into())),
    }
}

pub async fn read(db: &Database, id: &MealLedgerId) -> Result<Option<MealLedger>, AppError> {
    let row = sqlx::query_as!(
        MealLedger,
        "SELECT id AS \"id: MealLedgerId\", student AS \"student: UserId\", kind AS \"kind: MealLedgerKind\", amount_minor AS \"amount_minor: LedgerAmount\", source, method AS \"method: LedgerMethod\", note AS \"note: LedgerNote\", recorded_by AS \"recorded_by: UserId\", created_at AS \"created_at: Timestamp\"
         FROM meal_ledger WHERE id = $1",
        id.key(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
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
    let row =
        sqlx::query!("SELECT CAST(COALESCE(sum(price_minor), 0) AS BIGINT) AS \"total!\" FROM menu_dish WHERE menu = $1", menu.key())
            .fetch_one(db)
            .await?;
    let total = row.total;
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
        "meal_ledger WHERE student = $1",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind(student.uuid())
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
/// [`MealLedgerKind::sign`](crate::domain::meal_ledger::MealLedgerKind::sign)
/// the documented formula is spelled in. Summing a signed CASE in SQL would
/// have folded the whole balance in one statement and forked the one rule
/// that decides what money means into a second language, where nothing
/// would fail the day the two disagreed. Grouping instead keeps the
/// aggregate ignorant of signs: it counts kinds, and Rust still says what a
/// kind does. A stored running total was the third option and is a counter
/// that can drift — a bug class this repo closes, not one it opens.
pub async fn balance_of(db: &Database, student: &UserId) -> Result<i64, AppError> {
    let totals = sqlx::query!(
        r#"SELECT kind AS "kind: crate::domain::meal_ledger::MealLedgerKind",
                  CAST(sum(amount_minor) AS BIGINT) AS "total!"
           FROM meal_ledger WHERE student = $1 GROUP BY kind"#,
        student.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(totals
        .iter()
        .fold(0i64, |sum, row| sum + row.kind.sign() * row.total))
}
