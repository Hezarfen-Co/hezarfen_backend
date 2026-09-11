//! The `meal_attendance` table: the kitchen's marks — one atomic UPSERT per
//! (menu, student) that rides the menu's revision bump
//! ([`bump_menu_and_write`](crate::db::menu::bump_menu_and_write)), plus the
//! two listings. Attendance is reporting only: nothing here writes the
//! ledger, and the workflows that read it live in the web layer via
//! [`crate::service::meal_attendance`].

use surrealdb::types::SurrealValue;

use crate::database::Database;
use crate::db::menu::bump_menu_and_write;
use crate::db::page::PagedList;
use crate::domain::meal_attendance::{MealAttendance, MealAttendanceId, MealAttendanceStatus};
use crate::domain::menu::MenuDate;
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) whether `student` ate off `menu`. One row per
/// pair by construction, so a correction is the same UPSERT flipping the
/// status. No ledger line, ever — see the domain module header.
///
/// `NotFound` = the menu is gone, and the mark was *not* written. The mark
/// rides in [`bump_menu_and_write`], so the menu's existence is a *write* to
/// the menu row rather than a read the write then trusts: a delete racing
/// this one touches the very key this transaction bumps, and the store
/// refuses to commit both. Guarding by reading the menu — even as the
/// `UPSERT`'s own target — did not survive that race: the read saw a row
/// [`crate::db::menu::delete`] had deleted but not
/// yet committed, its sweep ran on a snapshot predating this insert, and the
/// mark outlived its menu with neither caller told anything (measured 378 of
/// 3600 raced rounds). Since [`MenuId::for_slot`] is deterministic,
/// republishing that day and slot then resurrected it as a mark on the new
/// menu.
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
    let row = MealAttendance {
        id: MealAttendanceId::composite(menu, student),
        menu: menu.clone(),
        student: student.clone(),
        status,
        marked_by: marked_by.clone(),
        marked_at: Timestamp::now(),
    };
    // Admissible for the retry loop inside: the id is bijective with the
    // (menu, student) pair the table indexes UNIQUE, so this `UPSERT`
    // resolves onto the row that index already points at instead of
    // colliding with it — it can never answer "already exists", and a
    // re-sent round writes the identical row.
    bump_menu_and_write(
        menu,
        "UPSERT $id CONTENT $row RETURN AFTER",
        vec![
            ("id".into(), row.id.record().into_value()),
            ("row".into(), row.into_value()),
        ],
        db,
    )
    .await?
    .ok_or_else(|| AppError::Internal("failed to record the mark".into()))
}

/// The kitchen's list for one menu.
pub async fn list_for_menu(
    db: &Database,
    menu: &MenuId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealAttendance>, i64), AppError> {
    PagedList::new(
        "meal_attendance WHERE menu = $menu",
        "ORDER BY marked_at DESC, id DESC",
    )
    .bind("menu", menu.record())
    .run(limit, offset, db)
    .await
}

/// One student's marks, newest first. `from`/`to` are inclusive
/// `YYYY-MM-DD` bounds on the *menu's* day — the comparison is lexical,
/// which is chronological for that format (see [`MenuDate`]), and the link
/// is followed rather than denormalized so a row can never disagree with
/// the menu it belongs to.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    from: Option<&MenuDate>,
    to: Option<&MenuDate>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealAttendance>, i64), AppError> {
    PagedList::new(
        "meal_attendance WHERE student = $usr \
         AND ($from = NONE OR menu.date >= $from) \
         AND ($to = NONE OR menu.date <= $to)",
        "ORDER BY marked_at DESC, id DESC",
    )
    .bind("usr", student.record())
    .bind("from", from.map(|date| date.as_str().to_string()))
    .bind("to", to.map(|date| date.as_str().to_string()))
    .run(limit, offset, db)
    .await
}
