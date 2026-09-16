//! Karne workflows: the report a student's dönem adds up to, and the freeze
//! that turns it into a record when the dönem is archived.
//!
//! Karne is *computed*, not stored (D8): per instance, the student's marks are
//! weighted by their exam kinds' settings weights and averaged; the average
//! takes a label from the school's grade bands; and the dönem's own number is
//! a single average over the instances, each weighted by its `ders_saati` (the
//! karne weight the şube set on the instance). The catalog's `course` row
//! contributes only its title — the two şubeler teaching it are their own
//! instances and their own karne lines.
//!
//! [`build`] serves a frozen report back once its dönem is archived
//! ([`crate::db::karne`]), and computes live for an open one. [`freeze`] is the
//! write half, called from [`crate::service::term::archive`]: from that moment
//! a later correction to a mark no longer rewrites what a family holds.
//!
//! Whether a grade passed is read from the bands, never hardcoded: the
//! threshold is the `min` of the band labelled `"2"` (the Turkish ladder's
//! failing band, below it), so a school that edits its bands edits its verdict
//! with them.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::database::Database;
use crate::domain::class_group::ClassGroupId;
use crate::domain::term::{Term, TermId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// One instance's line on a karne: the average of the marks the student holds
/// in it this dönem, its band label, and the `ders_saati` it weighs into the
/// year average with. `course` is the catalog course's title (what a family
/// reads); `class_course` is the instance itself, for anything that must act
/// on the line.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct KarneInstance {
    pub class_course: String,
    pub course: String,
    pub ders_saati: i64,
    /// The instance's weighted average; `null` while nothing is graded in it.
    pub average: Option<f64>,
    /// The average's label from the school's grade bands; `null` with no
    /// average, or when the school configured no bands.
    pub band: Option<String>,
}

/// A student's karne for one dönem: every instance of their şubeler that
/// counts toward the karne, the `ders_saati`-weighted average across them, and
/// the verdict.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct KarneReport {
    pub user: String,
    pub term: String,
    pub instances: Vec<KarneInstance>,
    /// The instances' average, each weighted by its `ders_saati`; `null` while
    /// no instance has a graded mark.
    pub year_average: Option<f64>,
    /// `gecti` / `kaldi`, judged against the school's bands; `null` while
    /// there is no average, or when the school's bands state no passing floor.
    pub verdict: Option<String>,
}

/// The student's karne for `term`.
///
/// An archived dönem serves its frozen snapshot when one exists — the record
/// the school issued, which no later correction may rewrite. A term that was
/// never frozen (or is still open) computes live from the marks standing now,
/// which is what a snapshot-less archive must not silently serve: the caller
/// sees the computation, and [`freeze`] is what pins it.
pub async fn build(db: &Database, user: &UserId, term: &TermId) -> Result<KarneReport, AppError> {
    let term = crate::service::term::read(db, term)
        .await?
        .ok_or(AppError::NotFound)?;
    if term.is_archived()
        && let Some(payload) = crate::db::karne::read_snapshot(db, term.get_id(), user).await?
    {
        return serde_json::from_value(payload)
            .map_err(|e| AppError::Internal(format!("a karne snapshot is not a report: {e}")));
    }
    compute(db, user, &term).await
}

/// Write (or refresh) a frozen report for every student with a roster row
/// under `term`'s year — the archive half.
///
/// The students are the live rosters of the year's şubeler: an enrollment is
/// what puts a student under a şube, and the şube is what binds them to the
/// year the dönem is a slice of. Every such student gets exactly one snapshot
/// per dönem, computed by the same [`compute`] the live read uses, so the
/// frozen report and the served one can never disagree.
///
/// One snapshot write per student, one transaction each; the run pays a read
/// per şube rather than a join, because freeze runs once per archived dönem,
/// where the report it fixes is read for years.
pub async fn freeze(db: &Database, term: &TermId) -> Result<(), AppError> {
    let term = crate::service::term::read(db, term)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut students: Vec<UserId> = Vec::new();
    for class in crate::db::class_group::list_for_year(db, term.get_year()).await? {
        for instance in
            crate::db::class_course::list_for_class_ids(db, std::slice::from_ref(class.get_id()))
                .await?
        {
            let (roster, _) =
                crate::db::enrollment::list_for_class_course(db, instance.get_id(), None, 0)
                    .await?;
            for enrollment in roster {
                if !students.contains(enrollment.get_user()) {
                    students.push(*enrollment.get_user());
                }
            }
        }
    }
    for student in &students {
        let report = compute(db, student, &term).await?;
        let payload = serde_json::to_value(&report)
            .map_err(|e| AppError::Internal(format!("failed to freeze a karne: {e}")))?;
        crate::db::karne::snapshot(db, term.get_id(), student, &payload).await?;
    }
    Ok(())
}

/// The live computation: the student's şubeler in the dönem's year, their
/// karne-counting instances, and the marked exams of this dönem inside them.
async fn compute(db: &Database, user: &UserId, term: &Term) -> Result<KarneReport, AppError> {
    let school = crate::service::settings::load(db).await?;

    // The şubeler the student is live in *this year*: a membership from
    // another year is history and its marks belong to that year's karne.
    let (memberships, _) = crate::db::class_member::list_for_user(db, user, None, 0).await?;
    let member_classes: Vec<ClassGroupId> = memberships
        .iter()
        .map(|member| member.get_class().clone())
        .collect();
    let classes = crate::db::class_group::list_by_ids(db, &member_classes).await?;
    let class_ids: Vec<ClassGroupId> = classes
        .iter()
        .filter(|class| class.get_year() == Some(term.get_year()))
        .map(|class| class.get_id().clone())
        .collect();

    let instances: Vec<crate::domain::class_course::ClassCourse> =
        crate::db::class_course::list_for_class_ids(db, &class_ids)
            .await?
            .into_iter()
            .filter(|instance| instance.counts_toward_karne())
            .collect();
    let instance_ids: Vec<crate::domain::class_course::ClassCourseId> =
        instances.iter().map(|i| i.get_id().clone()).collect();

    // The dönem's exams inside those instances, plus this student's marks in
    // them. An exam another dönem owns is not this karne's to weigh.
    let exams: Vec<crate::domain::exam::Exam> =
        crate::db::exam::list_for_class_course_courses(db, &instance_ids)
            .await?
            .into_iter()
            .filter(|exam| exam.get_term() == term.get_id())
            .collect();
    let by_key: HashMap<String, &crate::domain::exam::Exam> = exams
        .iter()
        .map(|exam| (exam.get_id().key(), exam))
        .collect();
    let results = crate::db::exam_result::list_for_user_in_term(db, user, term.get_id()).await?;

    // Titles for the lines: one batch read for the whole report.
    let course_ids: Vec<crate::domain::course::CourseId> =
        instances.iter().map(|i| i.get_course().clone()).collect();
    let courses = crate::db::course::list_by_ids(db, &course_ids).await?;
    let titles: HashMap<String, String> = courses
        .iter()
        .map(|course| {
            (
                course.get_id().key(),
                course.get_title().as_str().to_string(),
            )
        })
        .collect();

    let mut lines = Vec::with_capacity(instances.len());
    let mut weighted: Vec<(f64, i64)> = Vec::new();
    for instance in &instances {
        let mut pairs: Vec<(i64, i64)> = Vec::new();
        for result in &results {
            let Some(exam) = by_key.get(result.get_exam().key().as_str()) else {
                // A mark for an exam of another şube's instance (or one whose
                // instance dropped off the list): not this line's.
                continue;
            };
            if exam.get_class_course() != instance.get_id() {
                continue;
            }
            // The kind's current settings weight; an exam keeps a retired
            // kind, and its marks then count once.
            let weight = school
                .exam_kind_weight(exam.get_kind().as_str())
                .unwrap_or(1);
            pairs.push((result.get_mark().as_i64(), weight));
        }
        let average = weighted_average(&pairs);
        if let Some(average) = average {
            weighted.push((average, instance.get_ders_saati().as_i64()));
        }
        lines.push(KarneInstance {
            class_course: instance.get_id().key(),
            course: titles
                .get(instance.get_course().key().as_str())
                .cloned()
                .unwrap_or_default(),
            ders_saati: instance.get_ders_saati().as_i64(),
            band: average.and_then(|average| school.grade_label(average).map(str::to_string)),
            average,
        });
    }

    let year_average = ders_saati_average(&weighted);
    Ok(KarneReport {
        user: user.key(),
        term: term.get_id().key(),
        instances: lines,
        year_average,
        verdict: verdict(&school, year_average),
    })
}

/// `Σ(mark×weight) / Σ(weight)`; `None` when there is nothing to average.
/// The zero-denominator guard is structural — kind weights are 1–100 and a
/// retired kind resolves to 1, so a weight is never 0. (The marks report's
/// own copy, [`crate::web::marks`]'s, weighs a single course; this one is the
/// same arithmetic as two free functions rather than one crossing the layer
/// line.)
fn weighted_average(pairs: &[(i64, i64)]) -> Option<f64> {
    let total_weight: i64 = pairs.iter().map(|(_, weight)| weight).sum();
    if total_weight == 0 {
        return None;
    }
    let total: i64 = pairs.iter().map(|(mark, weight)| mark * weight).sum();
    Some(total as f64 / total_weight as f64)
}

/// The single dönem number: each instance's average weighted by its
/// `ders_saati`. `None` while no instance has an average — a karne with
/// nothing graded has no verdict to give.
fn ders_saati_average(weighted: &[(f64, i64)]) -> Option<f64> {
    let total_weight: i64 = weighted.iter().map(|(_, saati)| saati).sum();
    if total_weight == 0 {
        return None;
    }
    let total: f64 = weighted
        .iter()
        .map(|(average, saati)| average * *saati as f64)
        .sum();
    Some(total / total_weight as f64)
}

/// The pass/fail verdict, read off the school's bands: the `min` of the band
/// labelled `"2"` is the passing floor (the label below a Turkish ladder's
/// pass). A school that lists no such band falls back to the lowest band that
/// still demands something (`min > 0`); a school that lists neither states no
/// floor, and the verdict is `null` rather than a guess.
fn verdict(school: &crate::domain::settings::Settings, average: Option<f64>) -> Option<String> {
    let average = average?;
    let bands = school.get_grade_bands();
    let floor = bands
        .iter()
        .find(|band| band.get_label() == "2")
        .or_else(|| {
            bands
                .iter()
                .filter(|band| band.get_min() > 0)
                .min_by_key(|band| band.get_min())
        })?
        .get_min();
    Some(if average >= floor as f64 {
        "gecti".to_string()
    } else {
        "kaldi".to_string()
    })
}
