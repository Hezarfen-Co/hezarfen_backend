//! The `holiday` table: the row mint, the read, the field-scoped PATCH whose
//! `WHERE` re-checks the merged range, the delete, and the two listings — a
//! paged calendar view and the materializer's overlap read. The type lives in
//! [`crate::domain::holiday`].

use crate::constant::HOLIDAY_TABLE;
use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::db::page::{Param, PagedList};
use crate::domain::holiday::{Holiday, HolidayId, HolidayKind, HolidayName};
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::UserId;
use crate::error::AppError;

/// A PATCH of one holiday row: only the fields the request carried are
/// written, `None` = omitted = keep.
#[derive(Debug, Clone, Default)]
pub struct HolidayPatch {
    pub name: Option<HolidayName>,
    pub starts_at: Option<Timestamp>,
    pub ends_at: Option<Timestamp>,
    pub kind: Option<HolidayKind>,
}

pub async fn create(
    db: &Database,
    name: &HolidayName,
    starts_at: Timestamp,
    ends_at: Timestamp,
    kind: &HolidayKind,
    creator: &UserId,
) -> Result<Holiday, AppError> {
    let holiday = Holiday {
        id: HolidayId::generate(),
        name: name.clone(),
        starts_at,
        ends_at,
        kind: kind.clone(),
        creator: *creator,
        created_at: Timestamp::now(),
    };
    let created = sqlx::query_as!(
        Holiday,
        r#"INSERT INTO holiday (id, name, starts_at, ends_at, kind, creator, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id AS "id: HolidayId", name AS "name: HolidayName",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                     kind AS "kind: HolidayKind", creator AS "creator: UserId",
                     created_at AS "created_at: Timestamp""#,
        holiday.id.uuid(),
        holiday.name.as_str(),
        holiday.starts_at.as_millis(),
        holiday.ends_at.as_millis(),
        holiday.kind.as_str(),
        holiday.creator.uuid(),
        holiday.created_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(created)
}

/// The holiday `id` names, or `Err(NotFound)` when absent.
pub async fn read(db: &Database, id: &HolidayId) -> Result<Holiday, AppError> {
    sqlx::query_as!(
        Holiday,
        r#"SELECT id AS "id: HolidayId", name AS "name: HolidayName",
                  starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                  kind AS "kind: HolidayKind", creator AS "creator: UserId",
                  created_at AS "created_at: Timestamp"
           FROM holiday WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Write only the fields the PATCH carried, and re-check the merged
/// `starts_at <= ends_at` in the `UPDATE`'s own `WHERE` — two PATCHes each
/// moving one end of the range cannot commit an inverted holiday between
/// them.
pub async fn update(
    db: &Database,
    id: &HolidayId,
    patch: HolidayPatch,
) -> Result<Holiday, AppError> {
    FieldUpdate::new(HOLIDAY_TABLE, id.uuid())
        .set("name", patch.name.map(|name| name.as_str().to_owned()))
        .set("starts_at", patch.starts_at.map(|at| at.as_millis()))
        .set("ends_at", patch.ends_at.map(|at| at.as_millis()))
        .set("kind", patch.kind.map(|kind| kind.as_str().to_owned()))
        .ordered("starts_at", "ends_at", range_error())
        .run::<Holiday>(db)
        .await
}

/// Delete the holiday. A holiday is referenced by nothing — the materializer
/// only *reads* it — so nothing can refuse the delete but its own absence.
pub async fn delete(db: &Database, id: &HolidayId) -> Result<Holiday, AppError> {
    let deleted = sqlx::query_as!(
        Holiday,
        r#"DELETE FROM holiday WHERE id = $1
           RETURNING id AS "id: HolidayId", name AS "name: HolidayName",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                     kind AS "kind: HolidayKind", creator AS "creator: UserId",
                     created_at AS "created_at: Timestamp""#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    deleted.ok_or(AppError::NotFound)
}

/// Holidays, newest start first, paged. `from`/`to` are inclusive bounds with
/// overlap semantics — a holiday is listed when it *reaches into* the range,
/// not only when it lies inside it.
pub async fn list(
    db: &Database,
    from: Option<Timestamp>,
    to: Option<Timestamp>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Holiday>, i64), AppError> {
    PagedList::new(
        format!(
            "{HOLIDAY_TABLE} WHERE ($1::bigint IS NULL OR ends_at >= $1) \
             AND ($2::bigint IS NULL OR starts_at <= $2)"
        ),
        "ORDER BY starts_at DESC, id DESC",
    )
    .bind(Param::OptI64(from.map(|at| at.as_millis())))
    .bind(Param::OptI64(to.map(|at| at.as_millis())))
    .run::<Holiday>(limit, offset, db)
    .await
}

/// Every holiday reaching into `[from, to]`, unpaged, earliest first — the
/// materializer's read. Same overlap semantics as [`list`].
pub async fn list_between(
    db: &Database,
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<Holiday>, AppError> {
    let rows = sqlx::query_as!(
        Holiday,
        r#"SELECT id AS "id: HolidayId", name AS "name: HolidayName",
                  starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                  kind AS "kind: HolidayKind", creator AS "creator: UserId",
                  created_at AS "created_at: Timestamp"
           FROM holiday
           WHERE ends_at >= $1 AND starts_at <= $2
           ORDER BY starts_at"#,
        from.as_millis(),
        to.as_millis(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}
