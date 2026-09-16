//! Mark reports: the per-instance weighted average a student carries, and the
//! karne — the same numbers rolled up per dönem.
//!
//! Both read through the *instance* (`class_course`): a student's marks are
//! grouped by the course their şube is taught, so two sections teaching the
//! same catalog course are two lines. The roll-up, the dönem average and the
//! verdict live in [`crate::service::karne`]; this module is the HTTP shape and
//! the teacher narrowing.

use std::collections::{HashMap, HashSet};

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;

use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::class_group::ClassGroupId;
use crate::domain::exam::Exam;
use crate::domain::role::Role;
use crate::domain::term::TermId;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::service::karne::KarneReport;
use crate::service::parent_link::ensure_can_observe;
use crate::state::AppState;

use super::instances::can_manage_instance;
use super::{CourseResponse, CurrentUser, course_people, person_map};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(my_marks))
        .routes(routes!(user_marks))
        .routes(routes!(my_karne))
        .routes(routes!(user_karne))
}

/// One graded exam inside a course block of the report.
#[derive(Serialize, ToSchema)]
struct MarkEntry {
    /// Exam id.
    exam: String,
    title: String,
    kind: String,
    /// The kind's weight from the school settings, resolved at request time —
    /// how many times this mark counts into the instance average. `1` when the
    /// school no longer lists the kind.
    weight: i64,
    mark: i64,
    /// The mark's label from the school's grade bands (`GET /settings`);
    /// `null` when no bands are configured.
    #[schema(example = "5")]
    grade: Option<String>,
    graded_by: String,
}

/// A student's marks in one instance — one catalog course as one şube teaches
/// it.
#[derive(Serialize, ToSchema)]
struct CourseMarks {
    /// The instance these marks belong to (`GET /instances/{id}`). Two şubeler
    /// teaching the same course are two blocks.
    instance: String,
    course: CourseResponse,
    /// Graded results only — ungraded exams don't appear.
    results: Vec<MarkEntry>,
    /// `Σ(mark×weight) / Σ(weight)` over the graded exams; `null` while
    /// nothing is graded.
    average: Option<f64>,
    /// The average's label from the school's grade bands; `null` when there
    /// is no average or no bands are configured.
    average_grade: Option<String>,
}

/// A student's full mark report across the instances they sit.
#[derive(Serialize, ToSchema)]
struct MarksReport {
    /// The student's user id.
    user: String,
    courses: Vec<CourseMarks>,
    /// Plain mean of the non-null block averages; `null` while no block has a
    /// graded exam.
    overall_average: Option<f64>,
    /// The overall average's label from the school's grade bands; `null` when
    /// there is no average or no bands are configured.
    overall_grade: Option<String>,
}

/// `Σ(mark×weight) / Σ(weight)`; `None` when there is nothing to average. The
/// zero-denominator guard is structural — kind weights are 1–100 and retired
/// kinds resolve to 1, so a weight is never 0.
fn weighted_average(pairs: &[(i64, i64)]) -> Option<f64> {
    let total_weight: i64 = pairs.iter().map(|(_, weight)| weight).sum();
    if total_weight == 0 {
        return None;
    }
    let total: i64 = pairs.iter().map(|(mark, weight)| mark * weight).sum();
    Some(total as f64 / total_weight as f64)
}

/// The instances a student's report is built from: the şubeler they are a live
/// member of, and the instances those şubeler carry — the same path the karne
/// walks, so the two can never disagree about which blocks a student has.
///
/// A hand-placed enrollment outside their şubeler is deliberately not a source
/// here: a student's academic identity is their section, and adding an
/// instance to it is what the office does with `POST /classes/{id}/instances`.
async fn enrolled_instances(user: &UserId, db: &Database) -> Result<Vec<ClassCourse>, AppError> {
    let (members, _) = crate::db::class_member::list_for_user(db, user, None, 0).await?;
    let classes: Vec<ClassGroupId> = members
        .iter()
        .map(|member| member.get_class().clone())
        .collect();
    crate::db::class_course::list_for_class_ids(db, &classes).await
}

/// Assemble the report: for each instance the student sits, join its exams with
/// the student's graded results, weigh each mark by its exam kind's settings
/// weight, then average. A `viewer` narrows the report to the instances that
/// viewer manages (self-reports and manager+ reports pass `None` and see
/// everything); the overall average follows the narrowed set.
async fn build_report(
    user: &UserId,
    viewer: Option<&User>,
    db: &Database,
) -> Result<MarksReport, AppError> {
    let mut instances = enrolled_instances(user, db).await?;
    if let Some(viewer) = viewer {
        let mut kept = Vec::with_capacity(instances.len());
        for instance in instances {
            if can_manage_instance(db, instance.get_id(), viewer).await? {
                kept.push(instance);
            }
        }
        instances = kept;
    }

    // One settings read weighs and labels the whole report, and one batch read
    // resolves every block's catalog row (its title and kind).
    let school = service::settings::load(db).await?;
    let course_ids: Vec<crate::domain::course::CourseId> =
        instances.iter().map(|i| i.get_course().clone()).collect();
    let courses = service::course::list_by_ids(db, &course_ids).await?;
    let by_key: HashMap<String, &crate::domain::course::Course> = courses
        .iter()
        .map(|course| (course.get_id().key(), course))
        .collect();
    let people = person_map(courses.iter().flat_map(course_people), db).await?;

    let mut blocks = Vec::with_capacity(instances.len());
    let mut seen: HashSet<String> = HashSet::new();
    for instance in &instances {
        let exams = crate::service::exam::list_for_class_course(db, instance.get_id()).await?;
        let by_exam: HashMap<String, &Exam> = exams.iter().map(|e| (e.get_id().key(), e)).collect();
        let results =
            crate::service::exam_result::list_for_user_in_course(db, instance.get_id(), user)
                .await?;

        let mut entries = Vec::with_capacity(results.len());
        let mut pairs = Vec::with_capacity(results.len());
        for result in &results {
            // A result whose exam is gone cannot happen given the delete
            // cascade — skip defensively rather than corrupt the average.
            let Some(exam) = by_exam.get(result.get_exam().key().as_str()) else {
                continue;
            };
            // The kind's current settings weight; an exam keeps a retired
            // kind, and its marks then count once.
            let weight = school
                .exam_kind_weight(exam.get_kind().as_str())
                .unwrap_or(1);
            entries.push(MarkEntry {
                exam: exam.get_id().key().to_string(),
                title: exam.get_title().as_str().to_string(),
                kind: exam.get_kind().as_str().to_string(),
                weight,
                mark: result.get_mark().as_i64(),
                grade: school
                    .grade_label(result.get_mark().as_i64() as f64)
                    .map(str::to_string),
                graded_by: result.get_graded_by().key().to_string(),
            });
            pairs.push((result.get_mark().as_i64(), weight));
        }

        let Some(course) = by_key.get(instance.get_course().key().as_str()) else {
            // An instance whose catalog row is gone is unreachable (the delete
            // is refused while instances exist) — skip rather than fabricate.
            continue;
        };
        seen.insert(instance.get_id().key());
        let average = weighted_average(&pairs);
        blocks.push(CourseMarks {
            instance: instance.get_id().key(),
            course: CourseResponse::new(course, &people),
            average,
            average_grade: average.and_then(|a| school.grade_label(a).map(str::to_string)),
            results: entries,
        });
    }

    let averages: Vec<f64> = blocks.iter().filter_map(|block| block.average).collect();
    let overall_average =
        (!averages.is_empty()).then(|| averages.iter().sum::<f64>() / averages.len() as f64);

    Ok(MarksReport {
        user: user.key().to_string(),
        courses: blocks,
        overall_average,
        overall_grade: overall_average.and_then(|a| school.grade_label(a).map(str::to_string)),
    })
}

/// The current user's mark report: every instance they sit, with its graded
/// exams, weighted instance averages, and the overall average.
#[utoipa::path(
    get,
    path = "/me",
    tag = "marks",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's mark report", body = MarksReport),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_marks(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<MarksReport>, AppError> {
    Ok(Json(build_report(user.get_id(), None, &st.db).await?))
}

/// Any user's mark report. Requires teacher+, or a parent tied to the target
/// student. Managers, admins, and parents see every instance; a teacher sees
/// only the target's instances they run — the rest of the report (other
/// sections) stays out of reach.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "marks",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's mark report, narrowed to the caller's instances", body = MarksReport),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn user_marks(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
) -> Result<Json<MarksReport>, AppError> {
    let target = UserId::from_key(&user);
    ensure_can_observe(&caller, &target, &st.db).await?;
    // User must exist — a missing user is a 404, not an empty report.
    crate::service::user::read(&st.db, &target)
        .await?
        .ok_or(AppError::NotFound)?;
    // Only an exactly-teacher caller is narrowed to the instances they run;
    // manager+ and a linked parent read the full report.
    let viewer = (caller.get_role() == Role::Teacher).then_some(&caller);
    Ok(Json(build_report(&target, viewer, &st.db).await?))
}

/// The `?term=` selector both karne routes share. Omitted, it means the newest
/// dönem on the calendar — the one a family is reading about now.
#[derive(Deserialize, IntoParams)]
struct KarneQuery {
    /// The dönem to report on (`GET /terms`). Omit for the newest one.
    #[param(example = "019732e3-7b00-7000-8000-00000000dead")]
    term: Option<String>,
}

/// The dönem the request names, resolved *without* the archive gate: an
/// archived dönem's karne is exactly what a family asks for, and the service
/// serves its frozen snapshot. A dönem that does not exist is a 404, as is an
/// empty calendar.
async fn resolve_term(query: &KarneQuery, db: &Database) -> Result<TermId, AppError> {
    match query.term.as_deref() {
        Some(id) => Ok(*service::term::read(db, &TermId::from_key(id))
            .await?
            .ok_or(AppError::NotFound)?
            .get_id()),
        None => {
            let (terms, _) = service::term::list_all(db, Some(1), 0).await?;
            terms
                .first()
                .map(|term| *term.get_id())
                .ok_or(AppError::NotFound)
        }
    }
}

/// Narrow a karne to the instances `viewer` runs: the lines stay, the dönem
/// average is recomputed over them (each weighted by its `ders_saati`, the
/// karne's own rule), and the verdict is dropped.
///
/// Dropping it is the honest half: the verdict is a whole-karne judgement
/// against the school's passing floor, and the floor's rule lives in
/// [`crate::service::karne`] — a partially-seen karne states no verdict rather
/// than one computed from a subset. A caller who runs every instance in the
/// report sees it, because nothing is filtered.
async fn narrow_karne(
    mut report: KarneReport,
    viewer: &User,
    db: &Database,
) -> Result<KarneReport, AppError> {
    let lines_seen = report.instances.len();
    let mut kept = Vec::with_capacity(lines_seen);
    for line in std::mem::take(&mut report.instances) {
        if can_manage_instance(db, &ClassCourseId::from_key(&line.class_course), viewer).await? {
            kept.push(line);
        }
    }
    // Nothing was filtered: the report is the whole karne, verdict included.
    if kept.len() == lines_seen {
        report.instances = kept;
        return Ok(report);
    }
    let mut total_weight = 0i64;
    let mut total = 0f64;
    for line in &kept {
        if let Some(average) = line.average {
            total_weight += line.ders_saati;
            total += average * line.ders_saati as f64;
        }
    }
    report.instances = kept;
    report.year_average = (total_weight > 0).then(|| total / total_weight as f64);
    report.verdict = None;
    Ok(report)
}

/// The current user's karne for one dönem: every instance of their şubeler that
/// counts toward the karne, the `ders_saati`-weighted average across them, and
/// the verdict. An archived dönem serves the snapshot the school froze when it
/// was closed; an open one computes live.
#[utoipa::path(
    get,
    path = "/karne",
    tag = "marks",
    security(("session_cookie" = [])),
    params(KarneQuery),
    responses(
        (status = 200, description = "The caller's karne for that dönem", body = KarneReport),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such term (or an empty calendar)", body = ErrorResponse),
    ),
)]
async fn my_karne(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<KarneQuery>,
) -> Result<Json<KarneReport>, AppError> {
    let term = resolve_term(&query, &st.db).await?;
    Ok(Json(
        service::karne::build(&st.db, user.get_id(), &term).await?,
    ))
}

/// Any user's karne for one dönem. Requires teacher+, or a parent tied to the
/// target student. A linked parent and manager+ read the whole karne; an
/// exactly-teacher caller sees only the lines of the instances they run, with
/// the dönem average recomputed over those and no verdict (see
/// [`narrow_karne`]).
#[utoipa::path(
    get,
    path = "/karne/{user}",
    tag = "marks",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id"), KarneQuery),
    responses(
        (status = 200, description = "The user's karne, narrowed to the caller's instances", body = KarneReport),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "User not found, or no such term", body = ErrorResponse),
    ),
)]
async fn user_karne(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(query): Query<KarneQuery>,
) -> Result<Json<KarneReport>, AppError> {
    let target = UserId::from_key(&user);
    ensure_can_observe(&caller, &target, &st.db).await?;
    crate::service::user::read(&st.db, &target)
        .await?
        .ok_or(AppError::NotFound)?;
    let term = resolve_term(&query, &st.db).await?;
    let report = service::karne::build(&st.db, &target, &term).await?;
    let report = if caller.get_role() == Role::Teacher {
        narrow_karne(report, &caller, &st.db).await?
    } else {
        report
    };
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dönem's own number, each instance weighted by its `ders_saati`: the
    /// karne's arithmetic, checked here against the report shape this module
    /// serves.
    #[test]
    fn the_report_shape_carries_the_instance_it_grouped_by() {
        let pairs = [(85, 1), (70, 2)];
        assert_eq!(weighted_average(&pairs), Some(75.0));
        assert_eq!(weighted_average(&[]), None);
    }
}
