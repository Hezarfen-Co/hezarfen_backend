//! The attendance-mark workflows: recording who ate off a menu and the two
//! listings the kitchen and the student report read. The mark itself is a
//! single atomic upsert that rides the menu's revision bump inside its own
//! transaction — the queries live in [`crate::db::meal_attendance`], the row
//! shape in [`crate::domain::meal_attendance`].

use crate::database::Database;
use crate::db::meal_attendance;
use crate::domain::meal_attendance::{MealAttendance, MealAttendanceStatus};
use crate::domain::menu::MenuDate;
use crate::domain::menu::MenuId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) whether `student` ate off `menu`. `NotFound` when
/// the menu is gone — the mark was not written.
pub async fn mark(
    db: &Database,
    menu: &MenuId,
    student: &UserId,
    status: MealAttendanceStatus,
    marked_by: &UserId,
) -> Result<MealAttendance, AppError> {
    meal_attendance::mark(db, menu, student, status, marked_by).await
}

/// The kitchen's list for one menu.
pub async fn list_for_menu(
    db: &Database,
    menu: &MenuId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealAttendance>, i64), AppError> {
    meal_attendance::list_for_menu(db, menu, limit, offset).await
}

/// One student's marks, newest first, within inclusive `YYYY-MM-DD` bounds
/// on the menu's day.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    from: Option<&MenuDate>,
    to: Option<&MenuDate>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<MealAttendance>, i64), AppError> {
    meal_attendance::list_for_student(db, student, from, to, limit, offset).await
}
