//! Event roll-call doors for the web layer. The mark is one atomic
//! transaction whose existence gate rides inside the write
//! ([`crate::db::attendance::mark`]), so this module is thin
//! pass-throughs; the event's audience gate is judged by the web layer
//! against the event it already read.

use crate::database::Database;
use crate::db::attendance;
use crate::domain::attendance::{Attendance, AttendanceStatus};
use crate::domain::event::EventId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) `user`'s status for `event`.
pub async fn mark(
    db: &Database,
    event: &EventId,
    user: &UserId,
    status: AttendanceStatus,
    marked_by: &UserId,
) -> Result<Attendance, AppError> {
    attendance::mark(db, event, user, status, marked_by).await
}

/// A page of the event's roll call, newest id first.
pub async fn list_for_event(
    db: &Database,
    event: &EventId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Attendance>, i64), AppError> {
    attendance::list_for_event(db, event, limit, offset).await
}

/// Every event-attendance row recorded for `user` — the events half of the
/// attendance report.
pub async fn list_for_user(db: &Database, user: &UserId) -> Result<Vec<Attendance>, AppError> {
    attendance::list_for_user(db, user).await
}

/// Clear one row; `None` = there was none.
pub async fn remove(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Attendance>, AppError> {
    attendance::remove(db, event, user).await
}
