use std::collections::HashMap;

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::Path;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::attendance::{Attendance, AttendanceStatus};
use crate::domain::course::{Course, CourseId};
use crate::domain::role::Role;
use crate::domain::session_attendance::SessionAttendance;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::courses::can_manage_course;
use super::{CourseResponse, CurrentUser, course_people, ensure_can_observe, person_map};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(my_report))
        .routes(routes!(user_report))
}

/// Per-status tallies over a set of attendance rows, plus the attendance rate.
#[derive(Serialize, ToSchema, Default)]
struct StatusCounts {
    present: u64,
    absent: u64,
    late: u64,
    excused: u64,
    /// Tallies of the school-added statuses (`GET /settings`), keyed by
    /// status. Rate-neutral like `excused`: their semantics are the school's,
    /// so they count into `total` only.
    custom: HashMap<String, u64>,
    /// All rows, `excused` and custom statuses included.
    total: u64,
    /// `(present + late) / (present + absent + late)` — being late is still
    /// attending, and an excused absence counts against no one. `null` when
    /// every row is excused/custom (or there are none).
    rate: Option<f64>,
}

impl StatusCounts {
    fn tally<'a>(statuses: impl Iterator<Item = &'a AttendanceStatus>) -> Self {
        let mut counts = StatusCounts::default();
        for status in statuses {
            counts.total += 1;
            match status.as_str() {
                "present" => counts.present += 1,
                "absent" => counts.absent += 1,
                "late" => counts.late += 1,
                "excused" => counts.excused += 1,
                custom => *counts.custom.entry(custom.to_string()).or_default() += 1,
            }
        }
        counts.rate = attendance_rate(counts.present, counts.absent, counts.late);
        counts
    }
}

/// `(present + late) / (present + absent + late)`; `None` when the denominator
/// is zero.
fn attendance_rate(present: u64, absent: u64, late: u64) -> Option<f64> {
    let counted = present + absent + late;
    (counted > 0).then(|| (present + late) as f64 / counted as f64)
}

/// A user's roll-call tallies in one course.
#[derive(Serialize, ToSchema)]
struct CourseAttendance {
    course: CourseResponse,
    counts: StatusCounts,
}

/// A user's full attendance report: generic events, lesson roll call overall,
/// and the roll call broken down per course.
#[derive(Serialize, ToSchema)]
struct AttendanceReport {
    /// The user's id.
    user: String,
    /// Tallies over event attendance (`/events/{id}/attendance`).
    events: StatusCounts,
    /// Tallies over every lesson roll-call row, all courses combined.
    sessions: StatusCounts,
    /// The session tallies split per course. Courses appear as long as the
    /// user has roll-call rows in them — attendance is a historical record,
    /// so unenrolling never hides an absence.
    courses: Vec<CourseAttendance>,
}

/// Assemble the report: tally the user's event rows, then their session rows
/// grouped by the (denormalized) course reference. A `viewer` narrows the
/// per-course blocks — and the overall session tally — to the courses that
/// viewer manages (self-reports and manager+ reports pass `None`). Event
/// tallies are school-wide, not course data, so they stay in either case.
async fn build_report(
    user: &UserId,
    viewer: Option<&User>,
    db: &Database,
) -> Result<AttendanceReport, AppError> {
    let events = Attendance::list_for_user(user, db).await?;
    let sessions = SessionAttendance::list_for_user(user, db).await?;

    // Group session rows by course, preserving first-seen (newest-first) order.
    let mut course_ids: Vec<CourseId> = Vec::new();
    let mut by_course: HashMap<String, Vec<&SessionAttendance>> = HashMap::new();
    for row in &sessions {
        let key = row.get_course().key().to_string();
        if !by_course.contains_key(&key) {
            course_ids.push(row.get_course().clone());
        }
        by_course.entry(key).or_default().push(row);
    }

    let courses = crate::service::course::list_by_ids(db, &course_ids).await?;
    let course_by_key: HashMap<&str, &Course> =
        courses.iter().map(|c| (c.get_id().key(), c)).collect();
    let people = person_map(courses.iter().flat_map(course_people), db).await?;

    let mut blocks = Vec::with_capacity(course_ids.len());
    let mut visible_rows: Vec<&SessionAttendance> = Vec::with_capacity(sessions.len());
    for course_id in &course_ids {
        // A row whose course is gone cannot happen given the delete cascade —
        // skip defensively rather than fabricate a course block.
        let Some(course) = course_by_key.get(course_id.key()) else {
            continue;
        };
        if viewer.is_some_and(|viewer| !can_manage_course(course, viewer)) {
            continue;
        }
        let rows = &by_course[course_id.key()];
        visible_rows.extend(rows.iter().copied());
        blocks.push(CourseAttendance {
            course: CourseResponse::new(course, &people),
            counts: StatusCounts::tally(rows.iter().map(|r| r.get_status())),
        });
    }

    // A narrowed viewer's overall tally follows the visible blocks; a full
    // report keeps the historical every-row tally.
    let session_counts = match viewer {
        Some(_) => StatusCounts::tally(visible_rows.iter().map(|r| r.get_status())),
        None => StatusCounts::tally(sessions.iter().map(|a| a.get_status())),
    };

    Ok(AttendanceReport {
        user: user.key().to_string(),
        events: StatusCounts::tally(events.iter().map(|a| a.get_status())),
        sessions: session_counts,
        courses: blocks,
    })
}

/// The current user's attendance report: event tallies, lesson roll-call
/// tallies, and a per-course breakdown with attendance rates.
#[utoipa::path(
    get,
    path = "/me",
    tag = "attendance",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's attendance report", body = AttendanceReport),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_report(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<AttendanceReport>, AppError> {
    Ok(Json(build_report(user.get_id(), None, &st.db).await?))
}

/// Any user's attendance report. Requires teacher+, or a parent tied to the
/// target student. Managers, admins, and parents see every course; a teacher
/// sees the event tallies plus only the roll-call blocks of the target's
/// courses they manage.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "attendance",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's attendance report, narrowed to the caller's courses", body = AttendanceReport),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn user_report(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
) -> Result<Json<AttendanceReport>, AppError> {
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
    async fn rate_counts_late_as_attended_and_ignores_excused() {
        // 3 present + 1 late out of 5 unexcused rows.
        assert_eq!(attendance_rate(3, 1, 1), Some(0.8));
        // Excused rows are outside both sides of the division.
        assert_eq!(attendance_rate(0, 0, 0), None);
        assert_eq!(attendance_rate(2, 0, 0), Some(1.0));
        assert_eq!(attendance_rate(0, 2, 0), Some(0.0));
    }

    #[tokio::test]
    async fn tally_buckets_every_status() {
        let allowed: Vec<String> = ["present", "absent", "late", "excused", "online"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let statuses: Vec<AttendanceStatus> =
            ["present", "present", "late", "absent", "excused", "online"]
                .iter()
                .map(|s| AttendanceStatus::try_new(s, &allowed).unwrap())
                .collect();
        let counts = StatusCounts::tally(statuses.iter());
        assert_eq!(counts.present, 2);
        assert_eq!(counts.late, 1);
        assert_eq!(counts.absent, 1);
        assert_eq!(counts.excused, 1);
        // A school-added status lands in its own bucket — never in `excused`.
        assert_eq!(counts.custom.get("online"), Some(&1));
        assert_eq!(counts.total, 6);
        // Rate ignores excused and custom rows: (2 + 1) / (2 + 1 + 1).
        assert_eq!(counts.rate, Some(0.75));
    }
}
