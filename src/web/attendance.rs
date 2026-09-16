//! Attendance reports: the event tallies, the lesson roll call per instance,
//! and the devamsızlık block a Turkish school actually reads — how many
//! *days* a student was away in each dönem, and whether that is over the
//! configured limit.
//!
//! The day counts are bucketed in the school's timezone
//! ([`Settings::get_timezone`](crate::domain::settings::Settings::get_timezone)),
//! because "a day of absence" is a calendar day at the school, not a UTC one:
//! a lesson at 00:30 local is the same school day as one at 23:00 the evening
//! before, and a UTC bucket would split it in two.

use std::collections::{HashMap, HashSet};

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::Path;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course::{Course, CourseId};
use crate::domain::role::Role;
use crate::domain::session_attendance::SessionAttendance;
use crate::domain::term::Term;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::service::parent_link::ensure_can_observe;
use crate::state::AppState;

use super::instances::can_manage_instance;
use super::{CourseResponse, CurrentUser, course_people, person_map};

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

/// A user's roll-call tallies in one instance.
#[derive(Serialize, ToSchema)]
struct CourseAttendance {
    /// The instance these tallies belong to (`GET /instances/{id}`) — the
    /// course as one şube teaches it.
    instance: String,
    course: CourseResponse,
    counts: StatusCounts,
}

/// The configured per-dönem absence limits, or `null` per limit when the
/// school set none.
#[derive(Serialize, ToSchema)]
struct AbsenceLimits {
    max_excused_days: Option<i64>,
    max_unexcused_days: Option<i64>,
}

/// One dönem's devamsızlık: how many days the student was away, split the way
/// a Turkish school splits it.
///
/// A *day* is a calendar day at the school on which at least one lesson was
/// missed — two absences in the same day count once, which is what the
/// regulation counts. `unexcused_days` is the `absent` count and
/// `excused_days` the `excused` one; a status the school added counts into
/// neither (its semantics are the school's own).
#[derive(Serialize, ToSchema)]
struct TermAbsence {
    /// The dönem (`GET /terms/{id}`).
    term: String,
    name: String,
    #[schema(example = 3)]
    absent_days: i64,
    #[schema(example = 1)]
    excused_days: i64,
    /// The unexcused absence days — the count the mazeretsiz limit watches.
    /// Equal to `absent_days`; both are published because a client should not
    /// have to know that.
    #[schema(example = 3)]
    unexcused_days: i64,
    limits: AbsenceLimits,
    /// Whether either configured limit is exceeded. `false` when no limit is
    /// configured.
    over_limit: bool,
}

/// A user's full attendance report: generic events, lesson roll call overall,
/// the roll call broken down per instance, and the per-dönem devamsızlık.
#[derive(Serialize, ToSchema)]
struct AttendanceReport {
    /// The user's id.
    user: String,
    /// Tallies over event attendance (`/events/{id}/attendance`).
    events: StatusCounts,
    /// Tallies over every lesson roll-call row, all instances combined.
    sessions: StatusCounts,
    /// The session tallies split per instance. An instance appears as long as
    /// the user has roll-call rows in it — attendance is a historical record,
    /// so unenrolling never hides an absence.
    courses: Vec<CourseAttendance>,
    /// The per-dönem absent-day counts, oldest dönem first. A lesson that falls
    /// in no dönem's range is not counted here (it still counts in the
    /// tallies).
    devamsizlik: Vec<TermAbsence>,
}

/// The school's day boundary as a fixed offset in minutes.
///
/// The allow-list in [`crate::constant::TIMEZONES`] names zones whose offset
/// is constant (Türkiye has run on UTC+3 with no DST since 2016), so the
/// backend buckets days without carrying a timezone database — the same
/// arithmetic the settings module's own list is built on. An unrecognised name
/// falls back to [`crate::constant::DEFAULT_TIMEZONE`]'s offset, which is what
/// an unset setting
/// gets anyway.
fn zone_offset_minutes(timezone: &str) -> i32 {
    match timezone {
        "UTC" => 0,
        _ => 3 * 60,
    }
}

/// The calendar day an instant falls on, in the school's zone.
fn zoned_day(millis: i64, offset_minutes: i32) -> chrono::NaiveDate {
    let shifted = millis + i64::from(offset_minutes) * 60_000;
    chrono::DateTime::from_timestamp_millis(shifted)
        .expect("timestamps are within chrono's representable range")
        .date_naive()
}

/// The dönem an instant falls in: the containing term with the latest start,
/// or `None` when the calendar does not cover it.
fn term_of(instant: i64, terms: &[Term]) -> Option<&Term> {
    terms
        .iter()
        .filter(|term| {
            term.get_starts_at().as_millis() <= instant && instant <= term.get_ends_at().as_millis()
        })
        .max_by_key(|term| term.get_starts_at().as_millis())
}

/// Assemble the report: tally the user's event rows, then their session rows
/// grouped by the (denormalized) instance. A `viewer` narrows the per-instance
/// blocks — and the overall session tally, and the devamsızlık — to the
/// instances that viewer runs (self-reports and manager+ reports pass `None`).
/// Event tallies are school-wide, not course data, so they stay in either case.
async fn build_report(
    user: &UserId,
    viewer: Option<&User>,
    db: &Database,
) -> Result<AttendanceReport, AppError> {
    let events = crate::service::attendance::list_for_user(db, user).await?;
    let sessions = crate::service::session_attendance::list_for_user(db, user).await?;

    // Group session rows by instance, preserving first-seen (newest-first)
    // order.
    let mut instance_ids: Vec<ClassCourseId> = Vec::new();
    let mut by_instance: HashMap<String, Vec<&SessionAttendance>> = HashMap::new();
    for row in &sessions {
        let key = row.get_class_course().key();
        if !by_instance.contains_key(&key) {
            instance_ids.push(row.get_class_course().clone());
        }
        by_instance.entry(key).or_default().push(row);
    }

    let instances = crate::db::class_course::list_by_ids(db, &instance_ids).await?;
    let instance_by_key: HashMap<String, &ClassCourse> = instances
        .iter()
        .map(|instance| (instance.get_id().key(), instance))
        .collect();
    let course_ids: Vec<CourseId> = instances
        .iter()
        .map(|instance| instance.get_course().clone())
        .collect();
    let courses = crate::service::course::list_by_ids(db, &course_ids).await?;
    let course_by_key: HashMap<String, &Course> =
        courses.iter().map(|c| (c.get_id().key(), c)).collect();
    let people = person_map(courses.iter().flat_map(course_people), db).await?;

    let mut blocks = Vec::with_capacity(instance_ids.len());
    let mut visible_rows: Vec<&SessionAttendance> = Vec::with_capacity(sessions.len());
    for instance_id in &instance_ids {
        // A row whose instance is gone cannot happen given the delete cascade —
        // skip defensively rather than fabricate a block.
        let Some(instance) = instance_by_key.get(instance_id.key().as_str()) else {
            continue;
        };
        if let Some(viewer) = viewer
            && !can_manage_instance(db, instance.get_id(), viewer).await?
        {
            continue;
        }
        let Some(course) = course_by_key.get(instance.get_course().key().as_str()) else {
            continue;
        };
        let rows = &by_instance[instance_id.key().as_str()];
        visible_rows.extend(rows.iter().copied());
        blocks.push(CourseAttendance {
            instance: instance.get_id().key(),
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

    let devamsizlik = absence_by_term(&visible_rows, &blocks, db).await?;

    Ok(AttendanceReport {
        user: user.key().to_string(),
        events: StatusCounts::tally(events.iter().map(|a| a.get_status())),
        sessions: session_counts,
        courses: blocks,
        devamsizlik,
    })
}

/// The devamsızlık block: for every dönem the visible rows reach, the count of
/// distinct school-days on which the student was `absent` (unexcused) and on
/// which they were `excused`.
///
/// The day is the *lesson's* day (`course_session.starts_at`), never when the
/// teacher got round to marking: a roll call taken a week late must not move an
/// absence into another dönem. The lessons are read one instance at a time
/// (they are the same instances the blocks are built from), and the terms once.
async fn absence_by_term(
    rows: &[&SessionAttendance],
    blocks: &[CourseAttendance],
    db: &Database,
) -> Result<Vec<TermAbsence>, AppError> {
    if rows.is_empty() || blocks.is_empty() {
        return Ok(Vec::new());
    }
    // One read per visible instance: every session it ever held, keyed by id.
    let mut starts_at: HashMap<String, i64> = HashMap::new();
    for block in blocks {
        let (held, _) = crate::service::course_session::list_for_class_course(
            db,
            &ClassCourseId::from_key(&block.instance),
            None,
            0,
        )
        .await?;
        for session in held {
            starts_at.insert(session.get_id().key(), session.get_starts_at().as_millis());
        }
    }

    let school = crate::service::settings::load(db).await?;
    let offset = zone_offset_minutes(school.get_timezone());
    let (terms, _) = crate::service::term::list_all(db, None, 0).await?;

    // A day counts once per (dönem, status class), which is what the
    // regulation counts.
    let mut unexcused: HashSet<(String, chrono::NaiveDate)> = HashSet::new();
    let mut excused: HashSet<(String, chrono::NaiveDate)> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut by_term: HashMap<String, &Term> = HashMap::new();
    for row in rows {
        let class = match row.get_status().as_str() {
            "absent" => &mut unexcused,
            "excused" => &mut excused,
            // Every other status is the school's own semantics; it is not a
            // day away.
            _ => continue,
        };
        let Some(start) = starts_at.get(row.get_session().key().as_str()) else {
            continue;
        };
        let Some(term) = term_of(*start, &terms) else {
            // A lesson outside every dönem's range is not this block's.
            continue;
        };
        let key = term.get_id().key();
        if !by_term.contains_key(&key) {
            by_term.insert(key.clone(), term);
            order.push(key.clone());
        }
        class.insert((key, zoned_day(*start, offset)));
    }

    let mut blocks_out = Vec::with_capacity(order.len());
    for key in order {
        let Some(term) = by_term.get(&key) else {
            continue;
        };
        let absent_days = unexcused
            .iter()
            .filter(|(term_id, _)| term_id == &key)
            .count() as i64;
        let excused_days = excused
            .iter()
            .filter(|(term_id, _)| term_id == &key)
            .count() as i64;
        let limits = AbsenceLimits {
            max_excused_days: school.get_max_excused_absent_days(),
            max_unexcused_days: school.get_max_unexcused_absent_days(),
        };
        let over_limit = limits
            .max_excused_days
            .is_some_and(|max| excused_days > max)
            || limits
                .max_unexcused_days
                .is_some_and(|max| absent_days > max);
        blocks_out.push(TermAbsence {
            term: key,
            name: term.get_name().as_str().to_string(),
            absent_days,
            excused_days,
            unexcused_days: absent_days,
            limits,
            over_limit,
        });
    }
    blocks_out.sort_by_key(|block| block.term.clone());
    Ok(blocks_out)
}

/// The current user's attendance report: event tallies, lesson roll-call
/// tallies, a per-instance breakdown with attendance rates, and the per-dönem
/// devamsızlık.
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
/// target student. Managers, admins, and parents see every instance; a teacher
/// sees the event tallies plus only the roll-call blocks — and the devamsızlık
/// days — of the instances they run.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "attendance",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's attendance report, narrowed to the caller's instances", body = AttendanceReport),
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
    // Only an exactly-teacher caller is narrowed to the instances they run;
    // manager+ and a linked parent read the full report.
    let viewer = (caller.get_role() == Role::Teacher).then_some(&caller);
    Ok(Json(build_report(&target, viewer, &st.db).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The day bucket is the school's day, not UTC's: 22:30 UTC in summer
    /// İstanbul is already tomorrow. This is the whole reason the timezone is
    /// read off the settings.
    #[test]
    fn the_day_bucket_follows_the_school_zone() {
        let at = crate::domain::timestamp::Timestamp::from_millis(1_767_216_600_000); // 2025-12-31T21:30Z
        assert_eq!(
            zoned_day(at.as_millis(), zone_offset_minutes("UTC")).to_string(),
            "2025-12-31"
        );
        assert_eq!(
            zoned_day(at.as_millis(), zone_offset_minutes("Europe/Istanbul")).to_string(),
            "2026-01-01"
        );
    }
}
