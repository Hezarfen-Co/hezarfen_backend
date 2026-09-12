use std::collections::HashMap;

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::Path;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;

use crate::domain::exam::Exam;
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::service::parent_link::ensure_can_observe;
use crate::state::AppState;

use super::courses::can_manage_course;
use super::{CourseResponse, CurrentUser, course_people, person_map};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(my_marks))
        .routes(routes!(user_marks))
}

/// One graded exam inside a course block of the report.
#[derive(Serialize, ToSchema)]
struct MarkEntry {
    /// Exam id.
    exam: String,
    title: String,
    kind: String,
    /// The kind's weight from the school settings, resolved at request time —
    /// how many times this mark counts into the course average. `1` when the
    /// school no longer lists the kind.
    weight: i64,
    mark: i64,
    /// The mark's label from the school's grade bands (`GET /settings`);
    /// `null` when no bands are configured.
    #[schema(example = "AA")]
    grade: Option<String>,
    graded_by: String,
}

/// A student's marks in one enrolled course.
#[derive(Serialize, ToSchema)]
struct CourseMarks {
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

/// A student's full mark report across their enrolled courses.
#[derive(Serialize, ToSchema)]
struct MarksReport {
    /// The student's user id.
    user: String,
    courses: Vec<CourseMarks>,
    /// Plain mean of the non-null course averages; `null` while no course has
    /// a graded exam.
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

/// Assemble the report: for each enrolled course, join the course's exams
/// with the user's graded results, weigh each mark by its exam kind's
/// settings weight, then average. A `viewer` narrows the report to the
/// courses that viewer manages (self-reports and manager+ reports pass `None`
/// and see everything); the overall average follows the narrowed set.
async fn build_report(
    user: &UserId,
    viewer: Option<&User>,
    db: &Database,
) -> Result<MarksReport, AppError> {
    let (mut courses, _) = crate::service::course::list_enrolled(db, user, None, 0).await?;
    if let Some(viewer) = viewer {
        courses.retain(|course| can_manage_course(course, viewer));
    }

    // One settings read weighs and labels the whole report.
    let school = service::settings::load(db).await?;
    let people = person_map(courses.iter().flat_map(course_people), db).await?;

    let mut blocks = Vec::with_capacity(courses.len());
    for course in &courses {
        let exams = crate::service::exam::list_for_course(db, course.get_id()).await?;
        let by_key: HashMap<String, &Exam> = exams.iter().map(|e| (e.get_id().key(), e)).collect();

        let results =
            crate::service::exam_result::list_for_user_in_course(db, course.get_id(), user).await?;
        let mut entries = Vec::with_capacity(results.len());
        let mut pairs = Vec::with_capacity(results.len());
        for result in &results {
            // A result whose exam is gone cannot happen given the delete
            // cascade — skip defensively rather than corrupt the average.
            let Some(exam) = by_key.get(result.get_exam().key().as_str()) else {
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

        let average = weighted_average(&pairs);
        blocks.push(CourseMarks {
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

/// The current user's mark report: every enrolled course with its graded
/// exams, weighted course averages, and the overall average.
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
/// student. Managers, admins, and parents see every course; a teacher sees
/// only the target's courses they manage — the rest of the report (other
/// teachers' courses) stays out of reach.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "marks",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's mark report, narrowed to the caller's courses", body = MarksReport),
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
    // Only an exactly-teacher caller is narrowed to their managed courses;
    // manager+ and a linked parent read the full report.
    let viewer = (caller.get_role() == Role::Teacher).then_some(&caller);
    Ok(Json(build_report(&target, viewer, &st.db).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn weighted_average_handles_empty_and_weights() {
        assert_eq!(weighted_average(&[]), None);
        assert_eq!(weighted_average(&[(70, 1)]), Some(70.0));
        // (50×1 + 90×3) / 4 = 80
        assert_eq!(weighted_average(&[(50, 1), (90, 3)]), Some(80.0));
    }
}
