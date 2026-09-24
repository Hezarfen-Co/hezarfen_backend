//! The `class_course` table: the course-axis attach entry point and the
//! instance reads behind `/instances`. The refusals-to-errors policy and the
//! detach that turns a zero-row sweep into a 404 live in
//! [`crate::service::class_course`]; the transaction itself is the pump's,
//! [`crate::db::class_pump`].
//!
//! An instance is the academic anchor now: it points at its grade-level
//! **offering** template, carries its own content *overrides* (title,
//! description, and the nullable `ders_saati` / `counts_toward_karne` — a
//! `NULL` inherits from the offering chain, so every read goes through
//! [`crate::service::class_course::resolve_policy`], never a blind unwrap),
//! the three `*_inherited` set flags, and the roster counter its own teachers
//! ([`crate::db::class_course_teacher`]). Reads speak the *bare uuid* id of
//! the instance; the (class, course) pair survives as the UNIQUE key the
//! attach gates on.

use crate::constant::CLASS_COURSE_TABLE;
use crate::database::Database;
use crate::db::class_pump::Attached;
use crate::db::page::PagedList;
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_course::{ClassCourse, ClassCourseId, DersSaati, OverrideField};
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The projection every instance read shares.
///
/// Three translations ride it: `source` is stored as the blueprint's surrogate
/// uuid while the domain speaks the grade label the API does (a hand attach,
/// source NULL, survives the LEFT JOIN as NULL), `ders_saati` is a *nullable*
/// `SMALLINT` column while [`DersSaati`] is an `i64` newtype — decoded
/// through a `bigint` cast so the type the driver checks is the one the
/// newtype names, its `NULL` meaning *inherit* — and the overrides/flags ride
/// along as their plain column shapes.
const INSTANCE_COLUMNS: &str = "cc.id, cc.class, cc.course, cc.offering, cc.attached_by, \
     b.grade_level AS source, cc.title, cc.description, \
     cc.ders_saati::bigint AS ders_saati, cc.counts_toward_karne, \
     cc.subjects_inherited, cc.exam_weights_inherited, cc.weekly_plan_inherited, \
     cc.enrollment_count, cc.attached_at";

/// The attach itself, with the refusals left *unmapped*.
///
/// A hand attach ([`crate::service::class_course::attach`]) turns each of them
/// into the error the route answers with, because one call is one course and a
/// refusal is that call's whole answer. A blueprint pump cannot: it runs
/// one of these per (class, course), and a course that does not fit one
/// section must not abort the other eleven — so it needs to *read* the
/// refusal and carry on ([`crate::domain::class_blueprint`]).
///
/// `source` is the provenance tag, and it is written by the same statement
/// that writes the link, so no attachment can exist without the answer to
/// "may a blueprint take this back". It is also *claimed* in that
/// transaction ([`Attached::SourceGone`]): a blueprint deleted while this
/// pump ran has already swept by that tag, so a row landing afterwards
/// would carry a name nothing can reach.
pub(crate) async fn attach_sourced(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    attached_by: &UserId,
    source: Option<&ClassBlueprintId>,
) -> Result<Attached<ClassCourse>, AppError> {
    crate::db::class_pump::attach_course(db, class, course, attached_by, source).await
}

/// The instances a class carries, newest first — by when they were attached,
/// the instance id breaking the tie (v7 ids sort in mint order, so the order
/// is total and reads as creation order within one instant).
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassCourse>, i64), AppError> {
    PagedList::new(
        format!(
            "(SELECT {INSTANCE_COLUMNS} FROM {CLASS_COURSE_TABLE} cc \
             LEFT JOIN class_blueprint b ON b.id = cc.source) \
             AS {CLASS_COURSE_TABLE} WHERE class = $1"
        ),
        "ORDER BY attached_at DESC, id DESC",
    )
    .bind(class.uuid())
    .run(limit, offset, db)
    .await
}

/// The instances one catalog course is taught in, newest first — the read
/// behind "which sections run this course".
pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassCourse>, i64), AppError> {
    PagedList::new(
        format!(
            "(SELECT {INSTANCE_COLUMNS} FROM {CLASS_COURSE_TABLE} cc \
             LEFT JOIN class_blueprint b ON b.id = cc.source) \
             AS {CLASS_COURSE_TABLE} WHERE course = $1"
        ),
        "ORDER BY attached_at DESC, id DESC",
    )
    .bind(course.uuid())
    .run(limit, offset, db)
    .await
}

/// Every instance held by any of `classes` (one query, unpaged) — the join
/// half of the class-layer reads that already hold class ids (a roster read,
/// a blueprint status diff). Ids that name no class are simply absent from
/// the result.
pub async fn list_for_class_ids(
    db: &Database,
    classes: &[ClassGroupId],
) -> Result<Vec<ClassCourse>, AppError> {
    if classes.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = classes.iter().map(ClassGroupId::uuid).collect();
    let rows = sqlx::query_as!(
        ClassCourse,
        r#"SELECT cc.id AS "id: ClassCourseId", cc.class AS "class: ClassGroupId",
                  cc.course AS "course: CourseId", cc.offering AS "offering: CourseOfferingId",
                  cc.attached_by AS "attached_by: UserId",
                  b.grade_level AS "source?: ClassBlueprintId",
                  cc.title AS "title?: CourseTitle",
                  cc.description AS "description?: CourseDescription",
                  cc.ders_saati::bigint AS "ders_saati?: DersSaati",
                  cc.counts_toward_karne,
                  cc.subjects_inherited, cc.exam_weights_inherited, cc.weekly_plan_inherited,
                  cc.enrollment_count,
                  cc.attached_at AS "attached_at: Timestamp"
           FROM class_course cc
           LEFT JOIN class_blueprint b ON b.id = cc.source
           WHERE cc.class = ANY($1)
           ORDER BY cc.attached_at DESC, cc.id DESC"#,
        &ids,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Load every instance behind `ids` (one query) — the join half of the reads
/// that already hold instance ids (a marks report's per-instance blocks).
pub async fn list_by_ids(
    db: &Database,
    ids: &[ClassCourseId],
) -> Result<Vec<ClassCourse>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<uuid::Uuid> = ids.iter().map(ClassCourseId::uuid).collect();
    let rows = sqlx::query_as!(
        ClassCourse,
        r#"SELECT cc.id AS "id: ClassCourseId", cc.class AS "class: ClassGroupId",
                  cc.course AS "course: CourseId", cc.offering AS "offering: CourseOfferingId",
                  cc.attached_by AS "attached_by: UserId",
                  b.grade_level AS "source?: ClassBlueprintId",
                  cc.title AS "title?: CourseTitle",
                  cc.description AS "description?: CourseDescription",
                  cc.ders_saati::bigint AS "ders_saati?: DersSaati",
                  cc.counts_toward_karne,
                  cc.subjects_inherited, cc.exam_weights_inherited, cc.weekly_plan_inherited,
                  cc.enrollment_count,
                  cc.attached_at AS "attached_at: Timestamp"
           FROM class_course cc
           LEFT JOIN class_blueprint b ON b.id = cc.source
           WHERE cc.id = ANY($1)
           ORDER BY cc.attached_at DESC, cc.id DESC"#,
        &keys,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// One instance, or `None` when the id names no row.
pub async fn read(db: &Database, id: &ClassCourseId) -> Result<Option<ClassCourse>, AppError> {
    let row = sqlx::query_as!(
        ClassCourse,
        r#"SELECT cc.id AS "id: ClassCourseId", cc.class AS "class: ClassGroupId",
                  cc.course AS "course: CourseId", cc.offering AS "offering: CourseOfferingId",
                  cc.attached_by AS "attached_by: UserId",
                  b.grade_level AS "source?: ClassBlueprintId",
                  cc.title AS "title?: CourseTitle",
                  cc.description AS "description?: CourseDescription",
                  cc.ders_saati::bigint AS "ders_saati?: DersSaati",
                  cc.counts_toward_karne,
                  cc.subjects_inherited, cc.exam_weights_inherited, cc.weekly_plan_inherited,
                  cc.enrollment_count,
                  cc.attached_at AS "attached_at: Timestamp"
           FROM class_course cc
           LEFT JOIN class_blueprint b ON b.id = cc.source
           WHERE cc.id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The class this instance belongs to, or `None` when the id names no row —
/// the seam every instance-scoped route reads the section (and, through it, the
/// year) off.
pub async fn class_of(db: &Database, id: &ClassCourseId) -> Result<Option<ClassGroupId>, AppError> {
    let row = sqlx::query!(
        r#"SELECT class AS "class: ClassGroupId" FROM class_course WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| row.class))
}

/// Set the instance's own policy overrides — the PATCH behind
/// `PATCH /instances/{id}`. Each `Some(_)` *sets* the column (a PATCH never
/// clears; clearing is [`clear_overrides`]'s job); `None` keeps what the
/// column holds.
///
/// No counter moves and no compare-and-set is needed: every field is an
/// independent scalar, so two PATCHes touching different ones cannot revert
/// each other. `Err(NotFound)` when the instance is gone — the same answer
/// the caller's own read gives one instant earlier.
pub async fn update(
    db: &Database,
    id: &ClassCourseId,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    staff: Option<DersSaati>,
    counts: Option<bool>,
) -> Result<ClassCourse, AppError> {
    // `i64`, cast down by the statement itself: the column is a nullable
    // `SMALLINT` and the newtype wraps an `i64`, so naming both types
    // explicitly is what keeps the driver's type check honest in either
    // direction. The text columns set-only via `COALESCE` for the same
    // never-clears rule.
    let hours = staff.map(|hours| hours.as_i64());
    let title = title.map(|title| title.as_str().to_string());
    let description = description.map(|d| d.as_str().to_string());
    let row = sqlx::query_as!(
        ClassCourse,
        r#"WITH updated AS (
               UPDATE class_course
                  SET title = COALESCE($2, title),
                      description = COALESCE($3, description),
                      ders_saati = COALESCE($4::bigint, ders_saati::bigint)::smallint,
                      counts_toward_karne = COALESCE($5, counts_toward_karne)
                WHERE id = $1
                RETURNING *)
           SELECT u.id AS "id: ClassCourseId", u.class AS "class: ClassGroupId",
                  u.course AS "course: CourseId", u.offering AS "offering: CourseOfferingId",
                  u.attached_by AS "attached_by: UserId",
                  b.grade_level AS "source?: ClassBlueprintId",
                  u.title AS "title?: CourseTitle",
                  u.description AS "description?: CourseDescription",
                  u.ders_saati::bigint AS "ders_saati?: DersSaati",
                  u.counts_toward_karne,
                  u.subjects_inherited, u.exam_weights_inherited, u.weekly_plan_inherited,
                  u.enrollment_count,
                  u.attached_at AS "attached_at: Timestamp"
           FROM updated u
           LEFT JOIN class_blueprint b ON b.id = u.source"#,
        id.uuid(),
        title,
        description,
        hours,
        counts,
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(row)
}

/// Clear the named overrides back to **inherit**: the scalars go `NULL` (the
/// offering's default, then the constants, apply again) and a set flag goes
/// back to `TRUE` (the offering's set is authoritative again — deleting the
/// section's own set rows rides the child-table lanes' reset). The empty
/// request is refused by the service before it gets here; a gone instance is
/// the usual 404.
///
/// The whole reset is one statement: the named clears are judged on the row
/// as it is *at the write*, so a PATCH landing between the caller's read and
/// this write cannot be silently reverted field-by-field.
pub async fn clear_overrides(
    db: &Database,
    id: &ClassCourseId,
    fields: &[OverrideField],
) -> Result<ClassCourse, AppError> {
    let title = fields.contains(&OverrideField::Title);
    let description = fields.contains(&OverrideField::Description);
    let staff = fields.contains(&OverrideField::DersSaati);
    let counts = fields.contains(&OverrideField::CountsTowardKarne);
    let subjects = fields.contains(&OverrideField::Subjects);
    let exam_weights = fields.contains(&OverrideField::ExamWeights);
    let weekly_plan = fields.contains(&OverrideField::WeeklyPlan);
    let row = sqlx::query_as!(
        ClassCourse,
        r#"WITH updated AS (
               UPDATE class_course SET
                   title = CASE WHEN $2 THEN NULL ELSE title END,
                   description = CASE WHEN $3 THEN NULL ELSE description END,
                   ders_saati = CASE WHEN $4 THEN NULL ELSE ders_saati END,
                   counts_toward_karne = CASE WHEN $5 THEN NULL ELSE counts_toward_karne END,
                   subjects_inherited = CASE WHEN $6 THEN TRUE ELSE subjects_inherited END,
                   exam_weights_inherited = CASE WHEN $7 THEN TRUE ELSE exam_weights_inherited END,
                   weekly_plan_inherited = CASE WHEN $8 THEN TRUE ELSE weekly_plan_inherited END
                WHERE id = $1
                RETURNING *)
           SELECT u.id AS "id: ClassCourseId", u.class AS "class: ClassGroupId",
                  u.course AS "course: CourseId", u.offering AS "offering: CourseOfferingId",
                  u.attached_by AS "attached_by: UserId",
                  b.grade_level AS "source?: ClassBlueprintId",
                  u.title AS "title?: CourseTitle",
                  u.description AS "description?: CourseDescription",
                  u.ders_saati::bigint AS "ders_saati?: DersSaati",
                  u.counts_toward_karne,
                  u.subjects_inherited, u.exam_weights_inherited, u.weekly_plan_inherited,
                  u.enrollment_count,
                  u.attached_at AS "attached_at: Timestamp"
           FROM updated u
           LEFT JOIN class_blueprint b ON b.id = u.source"#,
        id.uuid(),
        title,
        description,
        staff,
        counts,
        subjects,
        exam_weights,
        weekly_plan,
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(row)
}
