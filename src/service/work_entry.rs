//! Work-log surfacing: the check-in/check-out pair, the staff log, and the
//! manager corrections. The queries live in
//! [`crate::db::work_entry`] — both transitions are single atomic writes on
//! the deterministic open id (`INSERT IGNORE` in, take-and-refile out), so
//! this domain has no workflow of its own.

use crate::database::Database;
use crate::db::work_entry;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::domain::work_entry::WorkEntry;
use crate::domain::work_entry::WorkEntryId;
use crate::error::AppError;

/// Check in (`409` while already checked in).
pub async fn check_in(db: &Database, user: &UserId) -> Result<WorkEntry, AppError> {
    work_entry::check_in(db, user).await
}

/// Check out (`409` when not checked in).
pub async fn check_out(db: &Database, user: &UserId) -> Result<WorkEntry, AppError> {
    work_entry::check_out(db, user).await
}

pub async fn read(db: &Database, id: &WorkEntryId) -> Result<Option<WorkEntry>, AppError> {
    work_entry::read(db, id).await
}

/// Every stint of `user`, newest first — the open one included.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<WorkEntry>, i64), AppError> {
    work_entry::list_for_user(db, user, limit, offset).await
}

/// Persist corrected instants on a closed entry; `None` keeps the field.
pub async fn update(
    db: &Database,
    entry: WorkEntry,
    check_in: Option<Timestamp>,
    check_out: Option<Timestamp>,
) -> Result<WorkEntry, AppError> {
    work_entry::update(db, entry, check_in, check_out).await
}

pub async fn remove(db: &Database, id: &WorkEntryId) -> Result<Option<WorkEntry>, AppError> {
    work_entry::remove(db, id).await
}
