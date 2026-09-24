//! Offering workflows: the one invariant the web layer cannot express alone —
//! only a **class-delivered** course (`kind = 'course'`) has grade-level
//! offerings, because only such a course is ever attached to a class section
//! ([`crate::domain::course::CourseKind::is_class_delivered`]). A club or an
//! etüt is joined individually, has no grade, and so has no template to
//! inherit from.
//!
//! Everything else (the duplicate template's `offering_exists`, the in-use
//! delete guard) is the store's own answer, mapped in
//! [`crate::db::course_offering`]. The manager+ gate lives in the web layer.

use crate::database::Database;
use crate::db::{course, course_offering};
use crate::domain::class_course::DersSaati;
use crate::domain::course::{CourseDescription, CourseId, CourseTitle};
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::grade::GradeLevel;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Mint the grade-level template for a course. A course that is not
/// class-delivered is a 400 (there is no grade an offering could hang off);
/// a template that already exists is the db layer's 409 `offering_exists`.
#[allow(clippy::too_many_arguments)] // the create body, spelled field by field
pub async fn create(
    db: &Database,
    by: &UserId,
    course_id: &CourseId,
    grade: GradeLevel,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    default_ders_saati: Option<DersSaati>,
    default_counts_toward_karne: Option<bool>,
) -> Result<CourseOffering, AppError> {
    let course = course::read(db, course_id).await?.ok_or(AppError::NotFound)?;
    if !course.get_kind().is_class_delivered() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "course",
            reason: "only a ders has grade-level offerings — a club or etüt is joined individually, not attached",
        }));
    }
    course_offering::create(
        db,
        course.get_id(),
        grade,
        title,
        description,
        default_ders_saati,
        default_counts_toward_karne,
        by,
    )
    .await
}

/// One offering by id, or `None` when the id names no row — the read behind
/// `GET /offerings/{id}`.
pub async fn read(
    db: &Database,
    id: &CourseOfferingId,
) -> Result<Option<CourseOffering>, AppError> {
    course_offering::read(db, id).await
}

/// The offerings, optionally narrowed by course and/or grade ladder rung,
/// paged — the read behind `GET /offerings?course=&grade_level=`.
pub async fn list(
    db: &Database,
    course: Option<&CourseId>,
    grade: Option<GradeLevel>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseOffering>, i64), AppError> {
    course_offering::list(db, course, grade, limit, offset).await
}

/// Field-scoped PATCH: absent keeps, `null` clears back to inherit, a value
/// sets the override. Shape flows straight through to
/// [`course_offering::update`]; a gone id is a 404 there.
pub async fn update(
    db: &Database,
    id: &CourseOfferingId,
    title: Option<Option<CourseTitle>>,
    description: Option<Option<CourseDescription>>,
    default_ders_saati: Option<Option<DersSaati>>,
    default_counts_toward_karne: Option<Option<bool>>,
) -> Result<CourseOffering, AppError> {
    course_offering::update(db, id, title, description, default_ders_saati, default_counts_toward_karne)
        .await
}

/// Delete the template. In use by any instance → 409 `offering_in_use`; a
/// gone id → 404.
pub async fn delete(db: &Database, id: &CourseOfferingId) -> Result<(), AppError> {
    course_offering::delete(db, id).await
}
