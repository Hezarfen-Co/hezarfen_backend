//! The weekly plan surface: the timetable template's routes on both owners —
//! the grade-level offering (`/offerings/{id}/weekly-plan`, the office's
//! door) and the class×course instance (`/instances/{id}/weekly-plan`, the
//! D10 door), split into the two mount functions the router composes
//! (`offering_routes` under `/offerings`, `instance_routes` inside
//! `/instances`) so each carries its own module's gate.
//!
//! A slot is one weekday plus a minutes-past-midnight window (`1` = Monday
//! through `7` = Sunday, `540` = 09:00). Two slots of one owner may touch but
//! never overlap (409 `slot_overlap`); a plan holds at most
//! [`crate::constant::MAX_WEEKLY_SLOTS`] slots (409 `slot_cap`).
//! Writing any instance-side row flips `weekly_plan_inherited` to `false` in
//! the same statement, and `DELETE /instances/{id}/weekly-plan` (no slot id)
//! is the reset back to inherit — the resolved reads never merge the two
//! sets.
//!
//! **No scheduler.** Nothing generates dated `course_session` rows from a
//! weekly plan — `course_session` stays the per-lesson occurrence, written by
//! its own routes. These routes serve template + override + resolution only.

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::weekly_slot::{SlotMinute, Weekday, WeeklySlot, WeeklySlotId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::state::AppState;
use crate::web::tenant_state::State;

use super::instances::can_view_instance;
use super::{CurrentUser, RequireManager, RequireTeacher};

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
}

/// A weekly plan line the caller sent: the ISO weekday and the two
/// minutes-past-midnight bounds of the lesson window.
#[derive(Deserialize, ToSchema)]
struct CreateSlot {
    /// `1` = Monday through `7` = Sunday.
    weekday: i16,
    /// Minutes past midnight (`540` = 09:00) — the window opens here.
    starts_at: i64,
    /// Minutes past midnight — strictly after `starts_at`.
    ends_at: i64,
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
}

impl WeeklySlotDto {
    pub fn new(slot: &WeeklySlot) -> Self {
        Self {
            id: slot.get_id().key(),
            weekday: slot.get_weekday().get(),
            starts_at: slot.get_starts_at().get(),
            ends_at: slot.get_ends_at().get(),
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
/// minute bounds here, the non-empty window inside the domain `WeeklySlot`.
fn parse_slot(
    req: &CreateSlot,
) -> Result<(Weekday, SlotMinute, SlotMinute), crate::error::ValidationError> {
    Ok((
        Weekday::new(req.weekday)?,
        SlotMinute::new(req.starts_at)?,
        SlotMinute::new(req.ends_at)?,
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
        (status = 400, description = "Invalid weekday or minutes, or an empty window", body = ErrorResponse),
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
    let (weekday, starts_at, ends_at) = parse_slot(&req)?;
    let slot = service::weekly_slot::add_for_offering(
        &st.db,
        offering.get_id(),
        weekday,
        starts_at,
        ends_at,
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
        (status = 400, description = "Invalid weekday or minutes, or an empty window", body = ErrorResponse),
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
    let (weekday, starts_at, ends_at) = parse_slot(&req)?;
    let slot =
        service::weekly_slot::add_for_class(&st.db, instance.get_id(), weekday, starts_at, ends_at)
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
