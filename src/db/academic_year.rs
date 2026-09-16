//! The `academic_year` table: the row mint, the paged calendar listing, the
//! field-scoped PATCH whose `WHERE` re-checks the merged range, and the 0/0
//! delete guard.
//!
//! The year is the top of the academic calendar (D3): a şube belongs to a year
//! and a dönem ([`crate::db::term`]) is a grading slice inside it. Its two
//! counters are the delete guard — a year that still carries a şube or a dönem
//! is refused outright, because dropping it silently would strand the year's
//! structure. The rollover that reads and writes the şubeler lives one layer
//! up ([`crate::service::academic_year::rollover`]).

use sqlx::types::Json;

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::academic_year::{
    AcademicYear, AcademicYearId, AcademicYearName, GradePromotion,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Mint one year. The creator is a foreign key: a gone account is the same
/// `NotFound` the web layer's own read answers with, never an orphan row.
pub async fn create(
    db: &Database,
    creator: &UserId,
    name: AcademicYearName,
    starts_at: Timestamp,
    ends_at: Timestamp,
    grade_promotions: Vec<GradePromotion>,
) -> Result<AcademicYear, AppError> {
    let promotions = Json(grade_promotions);
    let created = sqlx::query_as!(
        AcademicYear,
        r#"INSERT INTO academic_year (id, name, starts_at, ends_at, creator, grade_promotions)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: AcademicYearId", name AS "name: AcademicYearName",
                     starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                     archived_at AS "archived_at: Timestamp", creator AS "creator: UserId",
                     grade_promotions AS "grade_promotions: Json<Vec<GradePromotion>>",
                     class_count, term_count"#,
        AcademicYearId::generate().uuid(),
        name.as_str(),
        starts_at.as_millis(),
        ends_at.as_millis(),
        creator.uuid(),
        promotions as _,
    )
    .fetch_one(db)
    .await;
    match created {
        Ok(year) => Ok(year),
        Err(err) if crate::database::foreign_key_violation(&err) => Err(AppError::NotFound),
        // The name carries a UNIQUE constraint; a duplicate is a legacy-year
        // naming clash. The web layer pre-checks it and answers 409 with a
        // readable message, so reaching here is a race — reported as the same
        // conflict rather than a 500.
        Err(err) if crate::database::unique_violation(&err).is_some() => Err(AppError::Conflict(
            "an academic year with that name already exists",
        )),
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &AcademicYearId) -> Result<Option<AcademicYear>, AppError> {
    let year = sqlx::query_as!(
        AcademicYear,
        r#"SELECT id AS "id: AcademicYearId", name AS "name: AcademicYearName",
                  starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                  archived_at AS "archived_at: Timestamp", creator AS "creator: UserId",
                  grade_promotions AS "grade_promotions: Json<Vec<GradePromotion>>",
                  class_count, term_count
           FROM academic_year WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(year)
}

/// Every year, newest first — the calendar is small by nature.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<AcademicYear>, i64), AppError> {
    PagedList::new("academic_year", "ORDER BY starts_at DESC, id DESC")
        .run::<AcademicYear>(limit, offset, db)
        .await
}

/// Write only the fields the PATCH carried — `None` means the request omitted
/// it, so the column keeps the value it holds rather than being re-stated from
/// the snapshot the handler read. Every column here is non-clearable, so
/// `COALESCE` is the whole rule: absent = keep.
///
/// The merged range is re-checked by the `WHERE` (`ends_at > starts_at` on the
/// values this write will leave behind), so two PATCHes each moving one end
/// cannot commit an inverted year between them. `Err(NotFound)` when the row
/// is gone.
pub async fn update(
    db: &Database,
    id: &AcademicYearId,
    name: Option<AcademicYearName>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
    grade_promotions: Option<Vec<GradePromotion>>,
) -> Result<AcademicYear, AppError> {
    let promotions = grade_promotions.map(Json);
    let starts_millis = starts_at.map(|at| at.as_millis());
    let ends_millis = ends_at.map(|at| at.as_millis());
    let updated = sqlx::query_as!(
        AcademicYear,
        r#"UPDATE academic_year
              SET name = COALESCE($2, name),
                  starts_at = COALESCE($3, starts_at),
                  ends_at = COALESCE($4, ends_at),
                  grade_promotions = COALESCE($5, grade_promotions)
            WHERE id = $1
              AND COALESCE($4, ends_at) > COALESCE($3, starts_at)
            RETURNING id AS "id: AcademicYearId", name AS "name: AcademicYearName",
                      starts_at AS "starts_at: Timestamp", ends_at AS "ends_at: Timestamp",
                      archived_at AS "archived_at: Timestamp", creator AS "creator: UserId",
                      grade_promotions AS "grade_promotions: Json<Vec<GradePromotion>>",
                      class_count, term_count"#,
        id.uuid(),
        name.as_ref().map(AcademicYearName::as_str),
        starts_millis,
        ends_millis,
        promotions as _,
    )
    .fetch_optional(db)
    .await?;
    match updated {
        Some(year) => Ok(year),
        // Either the row is gone or the merged range is inverted — the same
        // shape the caller's own pre-flight range check answers, one instant
        // later. Only the refusal path pays for the read that tells them
        // apart.
        None => match read(db, id).await? {
            Some(_) => Err(AppError::Validation(
                crate::error::ValidationError::Invalid {
                    field: "ends_at",
                    reason: "must be after starts_at",
                },
            )),
            None => Err(AppError::NotFound),
        },
    }
}

/// Delete the year, but only while no şube and no dönem links it — nothing
/// here unlinks or cascades. `false` = refused, nothing was written.
///
/// Both counts are read off the year's own row, so the check and the delete
/// are one conditional write on one record: a class or term write racing this
/// either claims first (and the delete is refused) or finds the row gone (and
/// is refused itself). The referencing foreign keys (`class_group.year`,
/// `term.year`, both `NO ACTION`) are the backstop behind the counters, not a
/// second guard. The `Err(NotFound)` keeps the answer a concurrent *delete*
/// used to get.
pub async fn delete(db: &Database, year: AcademicYear) -> Result<bool, AppError> {
    let gone = sqlx::query!(
        r#"DELETE FROM academic_year
           WHERE id = $1 AND class_count = 0 AND term_count = 0"#,
        year.get_id().uuid(),
    )
    .execute(db)
    .await?;
    if gone.rows_affected() > 0 {
        return Ok(true);
    }
    // Still linked or already gone: the one statement cannot tell those apart,
    // and only the refusal path pays for the read that can.
    match read(db, year.get_id()).await? {
        Some(_) => Ok(false),
        None => Err(AppError::NotFound),
    }
}

/// A real academic year for the fixtures of the tables that hang off one: a
/// dönem ([`crate::db::term::a_test_term`]) and, through it, every exam. Minted
/// per call — the name is unique — with a creator that is a real `app_user`
/// row, because `creator` is a foreign key.
#[cfg(test)]
pub(crate) async fn a_test_year(db: &Database) -> AcademicYearId {
    let creator = UserId::generate();
    // The last six hex digits of the uuid, not a fixed-width byte range: a
    // `key()` is 36 characters, so `[30..38]` — the range a `[30..]` suffix
    // suggests — panics before the fixture ever reaches the store.
    let suffix = &creator.key()[30..];
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, $2, 0, 'manager')",
    )
    .bind(creator.uuid())
    .bind(format!("year-fixture-{suffix}"))
    .execute(db)
    .await
    .unwrap();
    let year = crate::db::academic_year::create(
        db,
        &creator,
        AcademicYearName::try_new(&format!("year-{suffix}")).unwrap(),
        Timestamp::from_millis(0),
        Timestamp::from_millis(1),
        Vec::new(),
    )
    .await
    .unwrap();
    *year.get_id()
}
