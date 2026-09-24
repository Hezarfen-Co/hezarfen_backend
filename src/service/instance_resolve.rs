//! The composed resolved view of one instance — the one call every
//! instance-scoped display rides.
//!
//! Wave 1/2 landed each axis's own resolver; this module composes them, so a
//! read surface never re-implements a fallback chain. The scalar content runs
//! instance → offering → catalog (`title`/`description`) or → constant
//! (`ders_saati` 1, `counts_toward_karne` TRUE); the set axes stay
//! flag-driven, exactly as their own resolvers rule — **override-or-inherit,
//! never merge**.
//!
//! Everything here is composition: [`resolve`] calls
//! [`crate::service::class_course::resolve_policy`],
//! [`crate::service::offering_subject::resolved_for_instance`],
//! [`crate::service::exam_weight::resolved_for_class`] and
//! [`crate::service::weekly_slot::resolved_for_instance`] and adds only the
//! title/description chain (whose two upper levels are a plain row read).
//! [`resolved_content`] is the batch twin of that chain for report surfaces
//! that already batch (karne, marks, attendance): two queries per batch,
//! never one per instance.

use std::collections::HashMap;

use crate::database::Database;
use crate::domain::class_course::{ClassCourse, DersSaati};
use crate::domain::course::Course;
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::exam_weight::ExamWeight;
use crate::domain::subject::Subject;
use crate::domain::weekly_slot::WeeklySlot;
use crate::error::AppError;

/// One instance's whole effective content: the resolved scalars plus *how*
/// each axis was sourced. A client renders this verbatim and never
/// re-implements the chain — the `*_overridden` flags tell an override from
/// an inherited value, and the three `*_inherited` flags tell whose set
/// (this section's own, possibly empty, or the offering template's) each
/// resolved set answers from.
#[derive(Debug, Clone)]
pub struct ResolvedInstance {
    /// The display title: this section's override, else its offering's, else
    /// the catalog course's.
    pub title: String,
    /// `true` when `title` is this section's own override.
    pub title_overridden: bool,
    /// The display description, same chain as [`Self::title`].
    pub description: String,
    /// `true` when `description` is this section's own override.
    pub description_overridden: bool,
    /// The effective weekly hours: override, else offering default, else 1.
    pub ders_saati: DersSaati,
    /// `true` when the hours are this section's own override.
    pub ders_saati_overridden: bool,
    /// The effective report-card policy: override, else offering default,
    /// else counted.
    pub counts_toward_karne: bool,
    /// `true` when the policy is this section's own override.
    pub counts_toward_karne_overridden: bool,
    /// `false` = the section's own subject table is authoritative, including
    /// when empty; `true` = the offering's set applies.
    pub subjects_inherited: bool,
    /// The syllabus the section teaches, resolved override-or-inherit.
    pub subjects: Vec<Subject>,
    /// The same switch for the section's exam-kind weights.
    pub exam_weights_inherited: bool,
    /// The section's effective weight map — every kind at its resolved
    /// weight (own rows in force, else the offering → settings → 1 chain).
    pub exam_weights: Vec<ExamWeight>,
    /// The same switch for the section's weekly plan.
    pub weekly_plan_inherited: bool,
    /// The resolved plan: the offering's template week while inheriting, the
    /// section's own rows (possibly empty) once it overrode.
    pub weekly_plan: Vec<WeeklySlot>,
}

/// The title/description a batch of instances should display, keyed by
/// instance id key — the chain run once per batch: one offering read and one
/// catalog read for the whole set, then the instance's own override judged
/// in memory. An instance whose catalog row is unreachable renders an empty
/// string (the delete cascade makes that unreachable too; the karne lines
/// degrade rather than 500, the same as before this module existed).
pub async fn resolved_content(
    db: &Database,
    instances: &[&ClassCourse],
) -> Result<HashMap<String, ResolvedContent>, AppError> {
    let mut offering_ids: Vec<CourseOfferingId> = Vec::new();
    let mut course_ids: Vec<crate::domain::course::CourseId> = Vec::new();
    for instance in instances {
        if !offering_ids.contains(instance.get_offering()) {
            offering_ids.push(instance.get_offering().clone());
        }
        if !course_ids.contains(instance.get_course()) {
            course_ids.push(instance.get_course().clone());
        }
    }
    let offerings = crate::db::course_offering::list_by_ids(db, &offering_ids).await?;
    let courses = crate::db::course::list_by_ids(db, &course_ids).await?;
    let offering_by_key: HashMap<String, &CourseOffering> = offerings
        .iter()
        .map(|offering| (offering.get_id().key(), offering))
        .collect();
    let course_by_key: HashMap<String, &Course> = courses
        .iter()
        .map(|course| (course.get_id().key(), course))
        .collect();

    Ok(instances
        .iter()
        .map(|instance| {
            let template = offering_by_key
                .get(&instance.get_offering().key())
                .copied();
            let catalog = course_by_key.get(&instance.get_course().key()).copied();
            let title = instance
                .get_title()
                .or(template.and_then(|o| o.get_title()))
                .or_else(|| catalog.map(|c| c.get_title()))
                .map(|title| title.as_str().to_string())
                .unwrap_or_default();
            let description = instance
                .get_description()
                .or(template.and_then(|o| o.get_description()))
                .or_else(|| catalog.map(|c| c.get_description()))
                .map(|description| description.as_str().to_string())
                .unwrap_or_default();
            (
                instance.get_id().key().to_string(),
                ResolvedContent { title, description },
            )
        })
        .collect())
}

/// The display strings of one instance: its resolved title and description.
pub struct ResolvedContent {
    pub title: String,
    pub description: String,
}

/// Compose the whole resolved view of `instance` — the one call a read
/// surface pays instead of layering its own fallbacks. A gone offering or
/// catalog row is a `NotFound` (the `RESTRICT` foreign keys make that
/// unreachable; a missing row is never silently read as an empty template).
pub async fn resolve(
    db: &Database,
    instance: &ClassCourse,
) -> Result<ResolvedInstance, AppError> {
    let offering = crate::db::course_offering::read(db, instance.get_offering())
        .await?
        .ok_or(AppError::NotFound)?;
    let course = crate::db::course::read(db, instance.get_course())
        .await?
        .ok_or(AppError::NotFound)?;
    let policy = crate::service::class_course::resolve_policy(db, instance).await?;
    let subjects =
        crate::service::offering_subject::resolved_for_instance(db, instance).await?;
    let (exam_weights_inherited, exam_weights) =
        crate::service::exam_weight::resolved_for_class(db, instance).await?;
    let weekly_plan = crate::service::weekly_slot::resolved_for_instance(db, instance).await?;

    let title = instance
        .get_title()
        .or(offering.get_title())
        .unwrap_or_else(|| course.get_title());
    let description = instance
        .get_description()
        .or(offering.get_description())
        .unwrap_or_else(|| course.get_description());

    Ok(ResolvedInstance {
        title: title.as_str().to_string(),
        title_overridden: instance.get_title().is_some(),
        description: description.as_str().to_string(),
        description_overridden: instance.get_description().is_some(),
        ders_saati: policy.ders_saati,
        ders_saati_overridden: instance.get_ders_saati().is_some(),
        counts_toward_karne: policy.counts_toward_karne,
        counts_toward_karne_overridden: instance.counts_toward_karne().is_some(),
        subjects_inherited: instance.subjects_inherited(),
        subjects,
        exam_weights_inherited,
        exam_weights,
        weekly_plan_inherited: instance.weekly_plan_inherited(),
        weekly_plan,
    })
}
