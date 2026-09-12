//! The `meal_attendance` table: the kitchen's marks — one atomic UPSERT per
//! (menu, student) that rides the menu's revision bump inside its own
//! transaction, plus the two listings. Attendance is reporting only: nothing
//! here writes the ledger, and the workflows that read it live in the web
//! layer via [`crate::service::meal_attendance`].

use crate::database::{Database, tx_with_retry};
use crate::db::page::{PagedList, Param};
use crate::domain::meal_attendance::{MealAttendance, MealAttendanceStatus};
use crate::domain::menu::{MenuDate, MenuId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) whether `student` ate off `menu`. One row per
/// pair by construction (`ON CONFLICT (menu, student)`), so a correction is
/// the same upsert flipping the status. No ledger line, ever — see the
/// domain module header.
///
/// `NotFound` = the menu is gone, and the mark was *not* written. The mark
/// and the menu's revision bump share one transaction, and the bump is what
/// makes the menu's **existence** part of it: the `UPDATE` matches nothing
/// once the menu row is deleted, and Postgres commits a deleting and a
/// bumping transaction on the same row strictly one after the other — the
/// mark either lands before the delete's guard re-reads its counter or not
/// at all. (Under the old store the same guarantee needed the shared bump
/// shape because deletes and child writes could interleave with a read in
/// between.)
///
/// The bump costs a booking in flight one re-read (its seat claim asserts
/// the revision it priced against) — the same tax every dish write already
/// levies, and it lands after the cutoff on a meal being served, where
/// bookings are rare.
pub async fn mark(
    db: &Database,
    menu: &MenuId,
    student: &UserId,
    status: MealAttendanceStatus,
    marked_by: &UserId,
) -> Result<MealAttendance, AppError> {
    let marked_at = Timestamp::now();
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let menu_key = menu.key().to_string();
    let student = *student;
    let marked_by = *marked_by;
    tx_with_retry(db, false, async move |tx| {
        let bumped = sqlx::query!(
            "UPDATE menu SET version = COALESCE(version, 0) + 1 WHERE id = $1
             RETURNING 1 AS bumped",
            menu_key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if bumped.is_none() {
            return Err(AppError::NotFound);
        }
        let row = sqlx::query_as!(
            MealAttendance,
            "INSERT INTO meal_attendance (menu, student, status, marked_by, marked_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (menu, student) DO UPDATE
             SET status = EXCLUDED.status,
                 marked_by = EXCLUDED.marked_by,
                 marked_at = EXCLUDED.marked_at
             RETURNING menu AS \"menu: MenuId\", student AS \"student: UserId\",
                       status AS \"status: MealAttendanceStatus\", marked_by AS \"marked_by: UserId\",
                       marked_at AS \"marked_at: Timestamp\"",
            menu_key,
            student.uuid(),
            status.as_str(),
            marked_by.uuid(),
            marked_at.as_millis(),
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(row)
    })
    .await
}

/// The kitchen's list for one menu.
pub async fn list_for_menu(
    db: &Database,
    menu: &MenuId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealAttendance>, i64), AppError> {
    PagedList::new(
        "meal_attendance WHERE menu = $1",
        "ORDER BY marked_at DESC, menu DESC",
    )
    .bind(menu.key().to_string())
    .run(limit, offset, db)
    .await
}

/// One student's marks, newest first. `from`/`to` are inclusive
/// `YYYY-MM-DD` bounds on the *menu's* day — the comparison is lexical,
/// which is chronological for that format (see [`MenuDate`]), and the link
/// is followed rather than denormalized so a row can never disagree with
/// the menu it belongs to. A `NULL` bound means the bound is absent.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    from: Option<&MenuDate>,
    to: Option<&MenuDate>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealAttendance>, i64), AppError> {
    PagedList::new(
        "meal_attendance WHERE student = $1 \
         AND ($2::text IS NULL OR menu IN (SELECT id FROM menu WHERE date >= $2::text)) \
         AND ($3::text IS NULL OR menu IN (SELECT id FROM menu WHERE date <= $3::text))",
        "ORDER BY marked_at DESC, menu DESC",
    )
    .bind(student.uuid())
    .bind(Param::OptText(from.map(|date| date.as_str().to_string())))
    .bind(Param::OptText(to.map(|date| date.as_str().to_string())))
    .run(limit, offset, db)
    .await
}
