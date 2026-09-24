//! The weekly plan surface: the timetable template's routes on both owners —
//! the grade-level offering (`/offerings/{id}/weekly-plan`, the office's
//! door) and the class×course instance (`/instances/{id}/weekly-plan`, the
//! D10 door), split into the two mount functions the router composes
//! (`offering_routes` under `/offerings`, `instance_routes` inside
//! `/instances`) so each carries its own module's gate.
//!
//! A slot is one weekday plus a minutes-past-midnight window (`1` = Monday
//! through `7` = Sunday, `540` = 09:00), and may carry an optional lesson
//! topic. Two slots of one owner may touch but never overlap (409
//! `slot_overlap`); a plan holds at most
//! [`crate::constant::MAX_WEEKLY_SLOTS`] slots (409 `slot_cap`).
//! Writing any instance-side row flips `weekly_plan_inherited` to `false` in
//! the same statement, and `DELETE /instances/{id}/weekly-plan` (no slot id)
//! is the reset back to inherit — the resolved reads never merge the two
//! sets.
//!
//! **No scheduler** — but one explicit door: `POST
//! /instances/{id}/weekly-plan/materialize` expands the section's *resolved*
//! plan into dated `course_session` rows over a range, dry-run first and
//! bounded by the school's holiday calendar
//! ([`crate::service::course_session::materialize`]). Nothing generates
//! lessons on its own; a caller asks, and a second ask over the same range
//! writes nothing new.

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::calendar::zone_offset_minutes;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::course_session::SessionTopic;
use crate::domain::timestamp::Timestamp;
use crate::domain::weekly_slot::{SlotMinute, Weekday, WeeklySlot, WeeklySlotId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::state::AppState;
use crate::web::tenant_state::State;

use super::instances::can_view_instance;
use super::{
    CurrentUser, RequireManager, RequireTeacher, SessionResponse, check_not_past, check_time_range,
    person_map,
};

/// The route set mounted under `/offerings` (relative paths): the template
/// week's reads and the office's writes.
pub fn offering_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_offering_weekly_plan, add_offering_slot))
        .routes(routes!(remove_offering_slot))
}

/// The instance-side half: mounted inside `/instances` — the section's
/// resolved plan, its override writes, and the reset. The two `DELETE`s stay
/// in separate `routes!` calls — one call may register each HTTP method only
/// once.
pub fn instance_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_instance_weekly_plan, add_instance_slot))
        .routes(routes!(remove_instance_slot))
        .routes(routes!(reset_instance_weekly_plan))
        .routes(routes!(materialize_weekly_plan))
}

/// A weekly plan line the caller sent: the ISO weekday, the two
/// minutes-past-midnight bounds of the lesson window, and the optional topic
/// every lesson generated from it is stamped with.
#[derive(Deserialize, ToSchema)]
struct CreateSlot {
    /// `1` = Monday through `7` = Sunday.
    #[schema(example = 1)]
    weekday: i16,
    /// Minutes past midnight (`540` = 09:00) — the window opens here.
    #[schema(example = 540)]
    starts_at: i64,
    /// Minutes past midnight — strictly after `starts_at`.
    #[schema(example = 600)]
    ends_at: i64,
    /// What this line's lessons are about. The materializer stamps it onto
    /// every lesson it generates from this slot; omit it and a generated
    /// lesson takes the instance's resolved course title instead (then the
    /// literal `"Ders"`).
    #[schema(example = "Üslü sayılar", max_length = 200)]
    topic: Option<String>,
}

/// Public shape of one weekly plan slot. Times stay the minutes-past-midnight
/// form the request sent; `weekday` stays the integer (`1` = Monday).
#[derive(Serialize, ToSchema)]
pub struct WeeklySlotDto {
    pub id: String,
    /// `1` = Monday through `7` = Sunday.
    pub weekday: i16,
    /// Minutes past midnight — the window opens here.
    pub starts_at: i64,
    /// Minutes past midnight, strictly after `starts_at`.
    pub ends_at: i64,
    /// The topic this line's generated lessons carry; `null` = fall back to
    /// the instance's resolved title.
    pub topic: Option<String>,
}

impl WeeklySlotDto {
    pub fn new(slot: &WeeklySlot) -> Self {
        Self {
            id: slot.get_id().key(),
            weekday: slot.get_weekday().get(),
            starts_at: slot.get_starts_at().get(),
            ends_at: slot.get_ends_at().get(),
            topic: slot.get_topic().map(|topic| topic.as_str().to_string()),
        }
    }
}

/// The section's **resolved** weekly plan: whose rows are authoritative and
/// what they are — the offering's template week while
/// `weekly_plan_inherited` stands, the section's own rows (possibly empty)
/// once it overrode. Ordered weekday first, then start time.
#[derive(Serialize, ToSchema)]
pub struct InstanceWeeklyPlan {
    pub weekly_plan: Vec<WeeklySlotDto>,
    /// `true` = the offering's template week is authoritative; `false` = the
    /// section's own rows below, including when that list is empty.
    pub weekly_plan_inherited: bool,
}

async fn offering_or_404(key: &str, db: &Database) -> Result<CourseOffering, AppError> {
    service::course_offering::read(db, &CourseOfferingId::from_key(key))
        .await?
        .ok_or(AppError::NotFound)
}

/// The instance a path id names, or a 404 — the read arm mirrors
/// [`super::instances`]'s.
async fn instance_or_404(key: &str, db: &Database) -> Result<ClassCourse, AppError> {
    service::class_course::read(db, &ClassCourseId::from_key(key))
        .await?
        .ok_or(AppError::NotFound)
}

/// The body's slot, range-checked before anything is written: weekday and
/// minute bounds here, the non-empty window inside the domain `WeeklySlot`,
/// and the optional topic through its own newtype.
fn parse_slot(
    req: &CreateSlot,
) -> Result<(Weekday, SlotMinute, SlotMinute, Option<SessionTopic>), crate::error::ValidationError> {
    Ok((
        Weekday::new(req.weekday)?,
        SlotMinute::new(req.starts_at)?,
        SlotMinute::new(req.ends_at)?,
        req.topic
            .as_deref()
            .map(SessionTopic::try_new)
            .transpose()?,
    ))
}

/// List the offering's template week — weekday first, then start time. Any
/// signed-in session, the same visibility as the offering read itself.
#[utoipa::path(
    get,
    path = "/{id}/weekly-plan",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    responses(
        (status = 200, description = "The template week, ordered weekday then start time", body = [WeeklySlotDto]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Offering not found", body = ErrorResponse),
    ),
)]
async fn list_offering_weekly_plan(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<WeeklySlotDto>>, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    let slots = service::weekly_slot::resolved_for_offering(&st.db, offering.get_id()).await?;
    Ok(Json(slots.iter().map(WeeklySlotDto::new).collect()))
}

/// Add one slot to the offering's template week. Manager+. Refused with 409
/// `slot_overlap` when a template slot already occupies an intersecting
/// window on that weekday (an exact duplicate overlaps too), 409 `slot_cap`
/// when the week already holds the maximum.
#[utoipa::path(
    post,
    path = "/{id}/weekly-plan",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    request_body = CreateSlot,
    responses(
        (status = 201, description = "Slot added", body = WeeklySlotDto),
        (status = 400, description = "Invalid weekday or minutes, an empty window, or an over-long topic", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Offering not found", body = ErrorResponse),
        (status = 409, description = "Another slot of this week overlaps the window (`slot_overlap`), or the plan is at the slot cap (`slot_cap`)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn add_offering_slot(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<CreateSlot>,
) -> Result<(StatusCode, Json<WeeklySlotDto>), AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    let (weekday, starts_at, ends_at, topic) = parse_slot(&req)?;
    let slot = service::weekly_slot::add_for_offering(
        &st.db,
        offering.get_id(),
        weekday,
        starts_at,
        ends_at,
        topic,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(WeeklySlotDto::new(&slot))))
}

/// Drop one slot of the offering's template week. Manager+. A slot of another
/// offering, or one already gone, is a 404.
#[utoipa::path(
    delete,
    path = "/{id}/weekly-plan/{slot}",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Offering id"),
        ("slot" = String, Path, description = "Slot id"),
    ),
    responses(
        (status = 204, description = "Slot removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Offering or slot not found", body = ErrorResponse),
    ),
)]
async fn remove_offering_slot(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path((id, slot)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    service::weekly_slot::remove_for_offering(
        &st.db,
        offering.get_id(),
        &WeeklySlotId::from_key(&slot),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The section's resolved weekly plan plus whose rows it is. Visible to the
/// same eyes as the instance read: enrolled students, its teachers, its
/// şube's homeroom teacher, and managers/admins.
#[utoipa::path(
    get,
    path = "/{id}/weekly-plan",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    responses(
        (status = 200, description = "The resolved plan and the flag that picked it", body = InstanceWeeklyPlan),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, and not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn list_instance_weekly_plan(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<InstanceWeeklyPlan>, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    if !can_view_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only this instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view it",
        ));
    }
    let weekly_plan = service::weekly_slot::resolved_for_instance(&st.db, &instance).await?;
    Ok(Json(InstanceWeeklyPlan {
        weekly_plan: weekly_plan.iter().map(WeeklySlotDto::new).collect(),
        weekly_plan_inherited: instance.weekly_plan_inherited(),
    }))
}

/// Add one slot to the section's own week. Requires teacher+ and a right over
/// the instance; the write flips `weekly_plan_inherited` to `false` in the
/// same statement, so from here the section's own rows — not the offering's
/// template — are what every read resolves. Same overlap and cap refusals as
/// the offering side.
#[utoipa::path(
    post,
    path = "/{id}/weekly-plan",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = CreateSlot,
    responses(
        (status = 201, description = "Slot added; the section now overrides the template", body = WeeklySlotDto),
        (status = 400, description = "Invalid weekday or minutes, an empty window, or an over-long topic", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived; another slot of this week overlaps the window (`slot_overlap`); or the plan is at the slot cap (`slot_cap`)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn add_instance_slot(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSlot>,
) -> Result<(StatusCode, Json<WeeklySlotDto>), AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;
    let (weekday, starts_at, ends_at, topic) = parse_slot(&req)?;
    let slot =
        service::weekly_slot::add_for_class(
            &st.db,
            instance.get_id(),
            weekday,
            starts_at,
            ends_at,
            topic,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(WeeklySlotDto::new(&slot))))
}

/// Drop one slot of the section's own week. Requires teacher+ and a right
/// over the instance. The flag flip to `false` rides the delete in the same
/// statement — dropping the last own row leaves the section with an
/// authoritative *empty* plan, not a quiet return to the template
/// (`DELETE /{id}/weekly-plan` is the way back to inherit). A slot of another
/// instance, or one already gone, is a 404 that leaves the flag untouched.
#[utoipa::path(
    delete,
    path = "/{id}/weekly-plan/{slot}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Instance id"),
        ("slot" = String, Path, description = "Slot id"),
    ),
    responses(
        (status = 204, description = "Slot removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance or slot not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn remove_instance_slot(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, slot)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;
    service::weekly_slot::remove_for_class(
        &st.db,
        instance.get_id(),
        &WeeklySlotId::from_key(&slot),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reset the section's weekly plan to inherit — delete every slot of its own
/// week and flip `weekly_plan_inherited` back to `true` in one statement, so
/// the offering's template week is what every read resolves again.
/// Idempotent for a section that never overrode.
#[utoipa::path(
    delete,
    path = "/{id}/weekly-plan",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    responses(
        (status = 204, description = "The section's own week is gone; the offering's template is authoritative again"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn reset_instance_weekly_plan(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::class_course::require_open(&st.db, instance.get_id()).await?;
    service::weekly_slot::reset_for_class(&st.db, instance.get_id()).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- materialize: the plan, made real ----------------------------------------

/// One materialize call: the range to cover, and whether to write.
#[derive(Deserialize, ToSchema)]
struct MaterializeRequest {
    /// First instant to generate lessons from, UTC unix-milliseconds. The
    /// instant names a *school* day (the school's timezone), so a late-evening
    /// UTC instant is the next Turkish day.
    #[schema(example = 1_793_318_400_000_i64)]
    from: i64,
    /// Last instant to generate lessons through, UTC unix-milliseconds; must
    /// not precede `from`, and the span between the two may not exceed
    /// `max_materialize_days` (`GET /limits`).
    #[schema(example = 1_794_873_600_000_i64)]
    to: i64,
    /// `false` (the default) is a **dry run**: it reads, decides, and writes
    /// nothing, so a client shows the outcome before committing. `true` writes
    /// the lessons and answers with the rows that landed.
    #[serde(default)]
    #[schema(example = false)]
    apply: bool,
}

/// One non-teaching day the range reached into.
#[derive(Serialize, ToSchema)]
struct BlockedDayResponse {
    /// The calendar day, `YYYY-MM-DD`, in the school's timezone.
    #[schema(example = "2026-10-29")]
    date: String,
    /// The holiday blocking it (`GET /holidays`).
    #[schema(example = "29 Ekim Cumhuriyet Bayramı")]
    holiday: String,
}

/// What one materialize call did — or, on a dry run, would have done.
#[derive(Serialize, ToSchema)]
struct MaterializeReport {
    /// The range the call covered, UTC unix-milliseconds.
    from: i64,
    to: i64,
    /// Whether the call wrote: `false` for a dry run.
    applied: bool,
    /// The resolved weekly plan's size — the lines the range was expanded
    /// from.
    slots: usize,
    /// How many lessons the range *asked* for, before the holidays and the
    /// instants the section already holds were subtracted.
    candidates: usize,
    /// The rows the database actually inserted, empty on a dry run. Same shape
    /// as a hand-created lesson (`POST /instances/{id}/sessions`).
    created: Vec<SessionResponse>,
    /// Candidate instants the section already had a lesson at.
    skipped_existing: usize,
    /// Days a holiday reached into — one `blocked` entry each; a day counted
    /// here produces no lesson at all.
    skipped_holiday: usize,
    blocked: Vec<BlockedDayResponse>,
    /// How many days the range spanned, inclusive.
    range_days: i64,
}

/// Expand this section's **resolved** weekly plan into dated lessons over
/// `from`–`to`. Requires teacher+ and a right over the instance (the same door
/// as adding a slot, since this writes lessons).
///
/// Nothing is ever deleted or updated: the call is additive, and a second call
/// over the same range writes nothing (`UNIQUE (class_course, starts_at)`), so
/// retrying after a failure is safe. A weekday the plan has no slot for
/// produces nothing, and so does every day a holiday reaches into — those days
/// are named in `blocked`. Weekends are *not* special-cased: a school may run
/// Saturday, so only the plan and the calendar decide.
///
/// The generated lesson's teacher is the instance's **first assigned**
/// teacher, and its topic is the slot's own topic, else the instance's
/// resolved course title, else the literal `"Ders"`.
#[utoipa::path(
    post,
    path = "/{id}/weekly-plan/materialize",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = MaterializeRequest,
    responses(
        (status = 200, description = "What the call did, or would do on a dry run", body = MaterializeReport),
        (status = 400, description = "The range is inverted, lies in the past, spans more than a year, or would create more than `max_materialize_sessions` lessons", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived; the section's weekly plan is empty (`instance_has_no_weekly_plan`); or no teacher is assigned to it (`instance_has_no_teacher`)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn materialize_weekly_plan(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<MaterializeRequest>,
) -> Result<Json<MaterializeReport>, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    let from = Timestamp::from_millis(req.from);
    let to = Timestamp::from_millis(req.to);
    check_time_range(Some(from), Some(to))?;
    // The same rule a hand-created lesson's `starts_at` obeys: nothing is
    // scheduled to begin in the past. (`to` is deliberately not checked — a
    // range may legitimately end in the past when `from` does not, which is
    // how a school backfills a plan it only now entered.)
    check_not_past("from", Some(from))?;
    // The day-bucketing read `attendance` makes: a range's instants name
    // *school* days, and each lesson lands at its slot's minute of that local
    // day.
    let school = service::settings::load(&st.db).await?;
    let offset = zone_offset_minutes(school.get_timezone());

    let outcome =
        service::course_session::materialize(&st.db, &instance, from, to, req.apply, offset).await?;
    // The teacher every generated row names, in one query.
    let people = person_map(
        outcome.created.iter().map(|session| *session.get_teacher()),
        &st.db,
    )
    .await?;
    let created = outcome
        .created
        .iter()
        .map(|session| SessionResponse::new(session, &people))
        .collect();
    Ok(Json(MaterializeReport {
        from: from.as_millis(),
        to: to.as_millis(),
        applied: outcome.applied,
        slots: outcome.slots,
        candidates: outcome.candidates,
        created,
        skipped_existing: outcome.skipped_existing,
        skipped_holiday: outcome.skipped_holiday,
        blocked: outcome
            .blocked
            .iter()
            .map(|day| BlockedDayResponse {
                date: day.day.to_string(),
                holiday: day.holiday.as_str().to_string(),
            })
            .collect(),
        range_days: outcome.range_days,
    }))
}
