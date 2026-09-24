//! Report-card workflows: the report a student's term adds up to, and the
//! freeze that turns it into a record when the term is archived.
//!
//! The report card is *computed*, not stored (D8): per instance, the student's
//! marks are weighted by their exam kinds' settings weights and averaged; the
//! average takes a label from the school's grade bands; and the term's own
//! number is a single average over the instances, each weighted by its
//! `ders_saati` (the report-card weight the section set on the instance). The
//! title a line shows is the instance's **resolved** one (override → offering
//! → catalog, via [`crate::service::instance_resolve`]) — two class sections
//! teaching the same catalog course are their own instances and their own
//! report-card lines.
//!
//! A line's marks are the exams *addressed to* its instance
//! (`exam_audience`), not only the ones it owns: a shared exam announced to
//! several instances is each of their exams (D2), and its marks are a line of
//! every one of their report cards.
//!
//! [`build`] serves a frozen report back once its term is archived
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

/// One instance's line on a report card: the average of the marks the student
/// holds in it this term, its band label, and the `ders_saati` it weighs into
/// the year average with. `course` is the instance's **resolved** title — its
/// section's override, else its offering's, else the catalog course's (what a
/// family reads); `class_course` is the instance itself, for anything that
/// must act on the line.
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

/// A student's report card for one term: every instance of their class
/// sections that counts toward the report card, the `ders_saati`-weighted
/// average across them, and the verdict.
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

/// The student's report card for `term`.
///
/// An archived term serves its frozen snapshot when one exists — the record
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
/// The students are the live rosters of the year's class sections: an
/// enrollment is what puts a student under a class section, and the class
/// section is what binds them to the year the term is a slice of. Every such
/// student gets exactly one snapshot per term, computed by the same [`compute`]
/// the live read uses, so the frozen report and the served one can never
/// disagree.
///
/// One snapshot write per student, one transaction each; the run pays a read
/// per class section rather than a join, because freeze runs once per archived
/// term, where the report it fixes is read for years.
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

/// The live computation: the student's class sections in the term's year,
/// their report-card-counting instances, and the marked exams of this term
/// inside them — each mark under every instance it was addressed to, owner or
/// not.
async fn compute(db: &Database, user: &UserId, term: &Term) -> Result<KarneReport, AppError> {
    let school = crate::service::settings::load(db).await?;

    // The class sections the student is live in *this year*: a membership from
    // another year is history and its marks belong to that year's report card.
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

    // The report-card-counting instances, each with its **resolved** policy:
    // `counts_toward_karne` is nullable now (NULL = inherit the offering's
    // default, else counted), so the filter and the weights both go through
    // the resolver — a raw read would treat "not decided" as a decision.
    let mut counted: Vec<(
        crate::domain::class_course::ClassCourse,
        crate::service::class_course::InstancePolicy,
    )> = Vec::new();
    for instance in crate::db::class_course::list_for_class_ids(db, &class_ids).await? {
        let policy = crate::service::class_course::resolve_policy(db, &instance).await?;
        if policy.counts_toward_karne {
            counted.push((instance, policy));
        }
    }
    let instance_ids: Vec<crate::domain::class_course::ClassCourseId> =
        counted.iter().map(|(i, _)| i.get_id().clone()).collect();

    // The term's exams inside those instances, plus this student's marks on
    // them. An exam another term owns is not this report card's to weigh.
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
    // Each mark arrives under every instance it counts in: the read resolves
    // the exam's audience, so a shared exam's marks are the report card of
    // every instance it was announced to, not only of its owner.
    let results =
        crate::db::exam_result::list_for_user_in_term(db, user, term.get_id(), &instance_ids)
            .await?;

    // Titles for the lines: the sections' **resolved** titles (override →
    // offering → catalog), batched — one offering read + one catalog read
    // for the whole report.
    let line_refs: Vec<&crate::domain::class_course::ClassCourse> =
        counted.iter().map(|(i, _)| i).collect();
    let titles = crate::service::instance_resolve::resolved_content(db, &line_refs).await?;

    let mut lines = Vec::with_capacity(counted.len());
    let mut weighted: Vec<(f64, i64)> = Vec::new();
    for (instance, policy) in &counted {
        let mut pairs: Vec<(i64, i64)> = Vec::new();
        // The section's weight chain answer per kind, computed once — every
        // mark of a kind weighs the same inside one line.
        let mut line_weights: HashMap<String, i64> = HashMap::new();
        for (line, result) in &results {
            // Every mark the read attributed to another instance stays out of
            // this line; one line's marks are exactly those addressed under it.
            if line != instance.get_id() {
                continue;
            }
            let Some(exam) = by_key.get(result.get_exam().key().as_str()) else {
                // A mark whose exam left the list between the two reads (or a
                // term's exam that is no longer there): not this line's.
                continue;
            };
            // The kind's weight *in this section*: the section's own
            // override, else the offering's, else the settings weight, else 1
            // — a retired kind keeps counting once, an own set in force is
            // the whole answer, empty included.
            let kind = exam.get_kind().as_str();
            let weight = match line_weights.get(kind) {
                Some(weight) => *weight,
                None => {
                    let weight =
                        crate::service::exam_weight::resolve(db, instance, kind).await?;
                    line_weights.insert(kind.to_string(), weight);
                    weight
                }
            };
            pairs.push((result.get_mark().as_i64(), weight));
        }
        let average = weighted_average(&pairs);
        if let Some(average) = average {
            weighted.push((average, policy.ders_saati.as_i64()));
        }
        lines.push(KarneInstance {
            class_course: instance.get_id().key(),
            // The resolved display title: the section's override, else its
            // offering's, else the catalog's — never the bare catalog row.
            course: titles
                .get(instance.get_id().key().as_str())
                .map(|content| content.title.clone())
                .unwrap_or_default(),
            ders_saati: policy.ders_saati.as_i64(),
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

/// The single term number: each instance's average weighted by its
/// `ders_saati`. `None` while no instance has an average — a report card with
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared-exam rule, read off the report card: an exam addressed to a
    /// second instance is *that* instance's exam too (D2), so its mark is a
    /// line of both report cards while an exam addressed to its owner alone
    /// stays on the owner's line. Read through `exam.class_course` the second
    /// line would have held no mark at all.
    #[tokio::test]
    async fn an_ortak_exam_counts_into_every_instance_it_is_addressed_to() {
        let (db, _leases) = crate::database::init_test_db().await;
        let fixture = crate::db::exam_result::tests::an_ortak_exam_karne(&db).await;

        let report = build(&db, &fixture.student, &fixture.term).await.unwrap();
        assert_eq!(report.instances.len(), 2, "a line per şube's instance");

        let line = |instance: &crate::domain::class_course::ClassCourseId| {
            report
                .instances
                .iter()
                .find(|line| line.class_course == instance.key())
                .unwrap_or_else(|| panic!("no line for {}", instance.key()))
        };

        // The owner sits both exams; the addressed instance only the shared
        // one. Both kinds (`yazili`, `sozlu`) weigh 1 in the default settings.
        let owner = line(&fixture.owner);
        assert_eq!(
            owner.average,
            Some(
                (crate::db::exam_result::tests::ORTAK_MARK
                    + crate::db::exam_result::tests::OWNER_MARK) as f64
                    / 2.0
            ),
            "the owner's line averages its own mark and the ortak one"
        );
        assert_eq!(owner.band.as_deref(), Some("3"), "65 lands in the 3 band");

        let addressed = line(&fixture.addressed);
        assert_eq!(
            addressed.average,
            Some(crate::db::exam_result::tests::ORTAK_MARK as f64),
            "the ortak mark — and only it — counts under the second instance"
        );
        assert_eq!(addressed.band.as_deref(), Some("5"));

        // Each instance weighs one hour, so the term is their plain mean.
        assert_eq!(report.year_average, Some(75.0));
        assert_eq!(
            report.verdict.as_deref(),
            Some("gecti"),
            "75 clears the floor"
        );
    }

    /// A class-level weight moves the average: with the owner section's own
    /// set in force (`yazili` = 2), its ortak mark counts double —
    /// (85×2 + 45×1) / 3 — while the sibling section, still inheriting, keeps
    /// the plain settings weight and its plain mean. Read off the settings
    /// singleton both lines would have stayed at 65 and 85.
    #[tokio::test]
    async fn a_class_level_weight_changes_the_karne_average() {
        let (db, _leases) = crate::database::init_test_db().await;
        let fixture = crate::db::exam_result::tests::an_ortak_exam_karne(&db).await;

        let owner = crate::service::class_course::read(&db, &fixture.owner)
            .await
            .unwrap()
            .unwrap();
        crate::service::exam_weight::set_for_class(&db, owner.get_id(), "yazili", 2)
            .await
            .unwrap();

        let report = build(&db, &fixture.student, &fixture.term).await.unwrap();
        let line = |instance: &crate::domain::class_course::ClassCourseId| {
            report
                .instances
                .iter()
                .find(|line| line.class_course == instance.key())
                .unwrap_or_else(|| panic!("no line for {}", instance.key()))
        };

        assert_eq!(
            line(&fixture.owner).average,
            Some((2.0 * crate::db::exam_result::tests::ORTAK_MARK as f64
                + crate::db::exam_result::tests::OWNER_MARK as f64)
                / 3.0),
            "the ortak mark counts double under the section's own weight"
        );
        assert_eq!(
            line(&fixture.addressed).average,
            Some(crate::db::exam_result::tests::ORTAK_MARK as f64),
            "the sibling still inherits — a class weight never leaks across sections"
        );
    }
}
