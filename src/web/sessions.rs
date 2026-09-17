use std::collections::HashMap;

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::class_course::ClassCourse;
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};

use crate::domain::role::Role;
use crate::domain::session_attendance::{SessionAttendance, SessionAttendanceId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::service::instance::can_manage_instance;
use crate::state::AppState;

use super::instances::can_view_instance;
use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireTeacher, SessionResponse, check_not_past,
    check_time_range, person_map, set_or_clear,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_session, update_session, delete_session))
        .routes(routes!(mark_roll_call, list_roll_call))
        .routes(routes!(remove_roll_call))
}

#[derive(Deserialize, ToSchema)]
struct UpdateSession {
    #[schema(max_length = 200)]
    topic: Option<String>,
    /// Reassign the session's teacher. Omit to keep; must be teacher+.
    teacher_id: Option<String>,
    /// Unix-millisecond timestamp. Omit to keep the current value. A newly
    /// set value must not be in the past.
    starts_at: Option<i64>,
    /// Unix-millisecond timestamp. Omit to keep the current value; send `null`
    /// to clear it. A newly set value must not be in the past.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    ends_at: Option<Option<i64>>,
}

#[derive(Deserialize, ToSchema)]
struct MarkRollCall {
    /// One of the school's attendance statuses (`GET /settings`; the core
    /// four are `present`, `absent`, `late`, `excused`).
    #[schema(max_length = 50, example = "present")]
    status: String,
    /// Whose roll-call state to record: an enrolled student, or the session's
    /// teacher (the latter requires manager+).
    user_id: String,
}

#[derive(Serialize, ToSchema)]
struct SessionAttendanceResponse {
    id: String,
    session: String,
    class_course: String,
    /// Whose attendance this row records.
    user: PersonRef,
    status: String,
    /// Who recorded it.
    marked_by: PersonRef,
}

impl SessionAttendanceResponse {
    fn new(attendance: &SessionAttendance, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: SessionAttendanceId::composite(attendance.get_session(), attendance.get_user())
                .key(),
            session: attendance.get_session().key().to_string(),
            class_course: attendance.get_class_course().key().to_string(),
            user: PersonRef::resolve(people, attendance.get_user()),
            status: attendance.get_status().as_str().to_string(),
            marked_by: PersonRef::resolve(people, attendance.get_marked_by()),
        }
    }
}

/// Load a session and its instance together; a session whose instance is gone
/// cannot happen given the delete cascade, so both misses are plain 404s.
async fn session_with_instance(
    id: &str,
    db: &Database,
) -> Result<(CourseSession, ClassCourse), AppError> {
    let session = service::course_session::read(db, &CourseSessionId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    let instance = crate::service::class_course::read(db, session.get_class_course())
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((session, instance))
}

/// Whether `user` is the session's own teacher *and* still `teacher`+ today.
///
/// The `teacher` column is a historical fact like an instance's assignment (a
/// demotion never rewrites past sessions), so teaching a session grants nothing
/// once the account falls below `teacher` — see [`can_manage_instance`], which
/// carries the same floor.
fn is_live_session_teacher(session: &CourseSession, user: &User) -> bool {
    user.get_role().at_least(Role::Teacher) && session.is_teacher(user.get_id())
}

/// Who may take (or amend) a session's roll call: the session's own teacher,
/// or anyone who manages its instance (manager+, one of its teachers, or its
/// section's homeroom teacher) — in both cases only while still `teacher`+.
async fn can_roll_call(
    session: &CourseSession,
    instance: &ClassCourse,
    user: &User,
    db: &Database,
) -> Result<bool, AppError> {
    Ok(is_live_session_teacher(session, user)
        || can_manage_instance(db, instance.get_id(), user).await?)
}

// ---- sessions -------------------------------------------------------------

/// Fetch a single session by id. Visible to the instance's enrolled students, the
/// session's own teacher, the instance's teachers, and managers/admins.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session", body = SessionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the session's teacher, and without instance rights", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_session(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<SessionResponse>, AppError> {
    let (session, instance) = session_with_instance(&id, &st.db).await?;
    if !is_live_session_teacher(&session, &user)
        && !can_view_instance(&st.db, instance.get_id(), &user).await?
    {
        return Err(AppError::Forbidden(
            "only the instance's enrolled students, the session's teacher, its teachers, or a manager/admin can view this session",
        ));
    }
    let people = person_map([*session.get_teacher()], &st.db).await?;
    Ok(Json(SessionResponse::new(&session, &people)))
}

/// Update a session. Requires teacher+ with instance-management rights. Omitted
/// fields keep their value; an explicit `null` clears `ends_at`.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    request_body = UpdateSession,
    responses(
        (status = 200, description = "Updated session", body = SessionResponse),
        (status = 400, description = "Invalid fields, time range, newly set times in the past, or teacher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn update_session(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateSession>,
) -> Result<Json<SessionResponse>, AppError> {
    // Only values this request sets are held to the no-past rule — a kept
    // `starts_at` of a lesson already underway is legitimately past. A provided
    // `ends_at` sets the field, an explicit `null` clears it, an omitted one is
    // left alone.
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    check_not_past("starts_at", starts_at)?;
    let ends_at = req.ends_at.map(|update| update.map(Timestamp::from_millis));
    if let Some(ends_at) = ends_at {
        check_not_past("ends_at", ends_at)?;
    }
    let (session, instance) = session_with_instance(&id, &st.db).await?;
    if !can_manage_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only the instance's teachers, its şube's homeroom teacher, or a manager/admin can edit this session",
        ));
    }
    crate::service::class_course::require_open(&st.db, instance.get_id()).await?;

    let topic = req
        .topic
        .as_deref()
        .map(SessionTopic::try_new)
        .transpose()?;
    let teacher = match req.teacher_id {
        Some(ref key) => Some(
            *service::course_session::resolve_session_teacher(Some(key), &user, &st.db)
                .await?
                .get_id(),
        ),
        None => None,
    };
    // The end this request left out is only *read* for the range check — it is
    // never written back, so a concurrent move of it survives. Pre-flight only:
    // the write's own `WHERE` re-makes this check (the db layer's `update`).
    check_time_range(
        Some(starts_at.unwrap_or_else(|| session.get_starts_at())),
        ends_at.unwrap_or_else(|| session.get_ends_at()),
    )?;

    let updated =
        service::course_session::update(&st.db, session, teacher, topic, starts_at, ends_at)
            .await?;
    let people = person_map([*updated.get_teacher()], &st.db).await?;
    Ok(Json(SessionResponse::new(&updated, &people)))
}

/// Delete a session and its roll-call rows. Requires teacher+ with
/// instance-management rights.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_session(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let (session, instance) = session_with_instance(&id, &st.db).await?;
    if !can_manage_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only the instance's teachers, its şube's homeroom teacher, or a manager/admin can delete this session",
        ));
    }
    crate::service::class_course::require_open(&st.db, instance.get_id()).await?;
    service::course_session::delete(&st.db, session).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- roll call --------------------------------------------------------------

/// Record a user's roll-call state for a session. The session's teacher or a
/// manager of its instance marks **enrolled students** (only students attend
/// classes); marking the **session's teacher** requires manager+ (staff
/// presence is management's call, so a teacher can't mark themselves present).
/// Students never self-mark a lesson.
#[utoipa::path(
    post,
    path = "/{id}/attendance",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    request_body = MarkRollCall,
    responses(
        (status = 200, description = "Roll-call state recorded", body = SessionAttendanceResponse),
        (status = 400, description = "Invalid status, unknown user, target not a student, or not on the roster", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the session's teacher or a manager of its instance; or marking the teacher without manager+", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn mark_roll_call(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<MarkRollCall>,
) -> Result<Json<SessionAttendanceResponse>, AppError> {
    let (session, instance) = session_with_instance(&id, &st.db).await?;
    if !can_roll_call(&session, &instance, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only the session's teacher or a manager of its instance can take roll call",
        ));
    }
    crate::service::class_course::require_open(&st.db, instance.get_id()).await?;

    let school = service::settings::load(&st.db).await?;
    let status = AttendanceStatus::try_new(&req.status, school.get_attendance_statuses())?;
    let target = UserId::from_key(&req.user_id);

    let (attendance, target_user) =
        service::session_attendance::mark(&st.db, &session, &user, &target, status).await?;
    let people = PersonRef::map_of(&[&target_user, &user]);
    Ok(Json(SessionAttendanceResponse::new(&attendance, &people)))
}

/// List a session's roll call, paged via `?limit=&offset=` (omit `limit` for
/// the whole roster). Same rights as taking it: the session's teacher or a
/// manager of its instance — students see their own tallies via
/// `GET /attendance/me`.
/// Returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/attendance",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id"), PageParams),
    responses(
        (status = 200, description = "A page of the roll-call roster (all of it when unpaged)", body = Page<SessionAttendanceResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the session's teacher or a manager of its instance", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
    ),
)]
async fn list_roll_call(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SessionAttendanceResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (session, instance) = session_with_instance(&id, &st.db).await?;
    if !can_roll_call(&session, &instance, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only the session's teacher or a manager of its instance can list the roll call",
        ));
    }
    let (rows, total) =
        service::session_attendance::list_for_session(&st.db, session.get_id(), limit, offset)
            .await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|a| [*a.get_user(), *a.get_marked_by()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|a| SessionAttendanceResponse::new(a, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Remove a user's roll-call row from a session. Same rights as marking:
/// the session's teacher or a manager of its instance for students, manager+
/// for a staff row (any target holding teacher or higher).
#[utoipa::path(
    delete,
    path = "/{id}/attendance/{user}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Session id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the session's teacher or a manager of its instance; or removing a staff row without manager+", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn remove_roll_call(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let (session, instance) = session_with_instance(&id, &st.db).await?;
    if !can_roll_call(&session, &instance, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only the session's teacher or a manager of its instance can take roll call",
        ));
    }
    crate::service::class_course::require_open(&st.db, instance.get_id()).await?;
    let target = UserId::from_key(&target);
    service::session_attendance::remove(&st.db, &session, &user, &target).await?;
    Ok(StatusCode::NO_CONTENT)
}
