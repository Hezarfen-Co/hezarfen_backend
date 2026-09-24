//! Holiday workflows: the school-wide non-teaching calendar is plain CRUD —
//! the rows carry no refcounts, no archive stamp, no cross-table rule — so
//! every door here is a thin pass-through over [`crate::db::holiday`]. The one
//! workflow-shaped read is [`blocked_days`], the weekly-plan materializer's
//! "which of these instants may not hold a lesson" read.

use crate::database::Database;
use crate::db::holiday::{self, HolidayPatch};
use crate::domain::holiday::{Holiday, HolidayId, HolidayKind, HolidayName};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Declare a school-wide holiday. Manager-gated at the route; the range
/// ordering is re-checked by the web layer's pre-flight and again at any
/// PATCH by the `WHERE` in [`update`].
pub async fn create(
    db: &Database,
    name: &HolidayName,
    starts_at: Timestamp,
    ends_at: Timestamp,
    kind: &HolidayKind,
    creator: &UserId,
) -> Result<Holiday, AppError> {
    holiday::create(db, name, starts_at, ends_at, kind, creator).await
}

/// The holiday `id` names, or `Err(NotFound)`.
pub async fn read(db: &Database, id: &HolidayId) -> Result<Holiday, AppError> {
    holiday::read(db, id).await
}

/// Patch only what the request carried; the merged range is re-checked in the
/// `UPDATE`'s own `WHERE`.
pub async fn update(
    db: &Database,
    id: &HolidayId,
    patch: HolidayPatch,
) -> Result<Holiday, AppError> {
    holiday::update(db, id, patch).await
}

/// Delete the holiday. Nothing references it — the materializer only reads —
/// so the delete is unconditional.
pub async fn delete(db: &Database, id: &HolidayId) -> Result<Holiday, AppError> {
    holiday::delete(db, id).await
}

/// The paged calendar view, `from`/`to` with overlap semantics.
pub async fn list(
    db: &Database,
    from: Option<Timestamp>,
    to: Option<Timestamp>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Holiday>, i64), AppError> {
    holiday::list(db, from, to, limit, offset).await
}

/// Every holiday reaching into `[from, to]` — the materializer's read. A
/// read-only door: it never writes.
pub async fn blocked_days(
    db: &Database,
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<Holiday>, AppError> {
    holiday::list_between(db, from, to).await
}
