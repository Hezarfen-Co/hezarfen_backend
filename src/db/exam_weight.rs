//! The `offering_exam_weight` / `class_course_exam_weight` tables: per-kind
//! weight overrides under the grade-level template and the class section.
//!
//! Every class-owned write keeps the override flag honest **in the same
//! statement**: a data-modifying CTE flips `class_course.exam_weights_inherited`
//! to `FALSE` while the weight row is written (or dropped), so a crash between
//! "row changed" and "flag changed" is impossible and
//! `class_course`'s own module never enters the picture. The reset deletes the
//! section's rows and restores `TRUE` in one statement too — the shape
//! [`crate::db::class_course::clear_overrides`] uses for the scalar overrides.
//!
//! The chain that *reads* these rows lives in
//! [`crate::service::exam_weight`]; this file is the SQL executor only.

use crate::database::Database;
use crate::domain::class_course::ClassCourseId;
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::exam_weight::ExamWeight;
use crate::error::AppError;

/// The offering's own weight rows, by kind — the override set of the
/// grade-level template.
pub async fn list_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
) -> Result<Vec<ExamWeight>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT kind, weight::bigint AS "weight!: i64"
           FROM offering_exam_weight WHERE offering = $1
           ORDER BY kind"#,
        offering.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ExamWeight {
            kind: row.kind,
            weight: row.weight,
        })
        .collect())
}

/// One offering weight row, or `None` when the pair holds none — the second
/// step of [`crate::service::exam_weight::resolve`].
pub async fn offering_weight(
    db: &Database,
    offering: &CourseOfferingId,
    kind: &str,
) -> Result<Option<ExamWeight>, AppError> {
    let row = sqlx::query!(
        r#"SELECT kind, weight::bigint AS "weight!: i64"
           FROM offering_exam_weight WHERE offering = $1 AND kind = $2"#,
        offering.uuid(),
        kind,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| ExamWeight {
        kind: row.kind,
        weight: row.weight,
    }))
}

/// Upsert one weight on the template. A duplicate (offering, kind) pair
/// *rewrites* the weight — a PATCH of an existing kind is the normal path, not
/// a conflict.
pub async fn upsert_for_offering(
    db: &Database,
    offering: &CourseOfferingId,
    kind: &str,
    weight: &ExamWeight,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"INSERT INTO offering_exam_weight (offering, kind, weight)
           VALUES ($1, $2, $3::bigint::smallint)
           ON CONFLICT (offering, kind) DO UPDATE SET weight = EXCLUDED.weight"#,
        offering.uuid(),
        kind,
        weight.get_weight(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Drop one weight row from the template. `false` when the pair held no row —
/// the caller's 404.
pub async fn delete_from_offering(
    db: &Database,
    offering: &CourseOfferingId,
    kind: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query!(
        r#"DELETE FROM offering_exam_weight WHERE offering = $1 AND kind = $2"#,
        offering.uuid(),
        kind,
    )
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The section's own weight rows, by kind — authoritative exactly while
/// `exam_weights_inherited` is `FALSE`, **including when empty**.
pub async fn list_for_class(
    db: &Database,
    instance: &ClassCourseId,
) -> Result<Vec<ExamWeight>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT kind, weight::bigint AS "weight!: i64"
           FROM class_course_exam_weight WHERE class_course = $1
           ORDER BY kind"#,
        instance.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ExamWeight {
            kind: row.kind,
            weight: row.weight,
        })
        .collect())
}

/// One section weight row, or `None` — the first step of
/// [`crate::service::exam_weight::resolve`] while the section's own set is in
/// force.
pub async fn class_weight(
    db: &Database,
    instance: &ClassCourseId,
    kind: &str,
) -> Result<Option<ExamWeight>, AppError> {
    let row = sqlx::query!(
        r#"SELECT kind, weight::bigint AS "weight!: i64"
           FROM class_course_exam_weight WHERE class_course = $1 AND kind = $2"#,
        instance.uuid(),
        kind,
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| ExamWeight {
        kind: row.kind,
        weight: row.weight,
    }))
}

/// Upsert one weight on the section **and** take the set own — both in the
/// statement that writes the row, so `exam_weights_inherited` can never claim
/// "inherit" over a row the section actually carries. `Err(NotFound)` when the
/// instance is gone (the CTE's UPDATE finds no row).
pub async fn upsert_for_class(
    db: &Database,
    instance: &ClassCourseId,
    kind: &str,
    weight: &ExamWeight,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"WITH updated AS (
               UPDATE class_course
                  SET exam_weights_inherited = FALSE
                WHERE id = $1
                RETURNING id
           ), written AS (
               INSERT INTO class_course_exam_weight (class_course, kind, weight)
               SELECT $1, $2, $3::bigint::smallint
                WHERE EXISTS (SELECT 1 FROM updated)
               ON CONFLICT (class_course, kind) DO UPDATE SET weight = EXCLUDED.weight
               RETURNING class_course
           )
           SELECT u.id AS "id: ClassCourseId" FROM updated u"#,
        instance.uuid(),
        kind,
        weight.get_weight(),
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(())
}

/// Drop one weight row from the section, flipping the flag to `FALSE` **only
/// when a row was actually dropped** — a DELETE naming a kind the section
/// carries no row for mutates nothing and answers `false` (the caller's 404),
/// so a refused delete never changes the inheritance state. `Err(NotFound)`
/// when the instance itself is gone.
pub async fn delete_from_class(
    db: &Database,
    instance: &ClassCourseId,
    kind: &str,
) -> Result<bool, AppError> {
    let row = sqlx::query!(
        r#"WITH kicked AS (
               DELETE FROM class_course_exam_weight
                WHERE class_course = $1 AND kind = $2
               RETURNING class_course
           ), updated AS (
               UPDATE class_course
                  SET exam_weights_inherited =
                      CASE WHEN EXISTS (SELECT 1 FROM kicked)
                           THEN FALSE ELSE exam_weights_inherited END
                WHERE id = $1
                RETURNING exam_weights_inherited
           )
           SELECT u.exam_weights_inherited AS "inherited: bool",
                  (SELECT count(*)::bigint FROM kicked) AS "removed!: i64"
           FROM updated u"#,
        instance.uuid(),
        kind,
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(row.removed > 0)
}

/// The reset door: delete every section weight row and restore
/// `exam_weights_inherited = TRUE` in one statement — the offering's set (then
/// the settings, then 1) applies again. `Err(NotFound)` when the instance is
/// gone; otherwise idempotent.
pub async fn reset_class(db: &Database, instance: &ClassCourseId) -> Result<(), AppError> {
    sqlx::query!(
        r#"WITH kicked AS (
               DELETE FROM class_course_exam_weight WHERE class_course = $1
           ), updated AS (
               UPDATE class_course
                  SET exam_weights_inherited = TRUE
                WHERE id = $1
                RETURNING id
           )
           SELECT u.id AS "id: ClassCourseId" FROM updated u"#,
        instance.uuid(),
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(())
}
