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
use crate::domain::badge;
use crate::domain::course::Course;
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::session_attendance::SessionAttendance;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course};
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
    course: String,
    /// Whose attendance this row records.
    user: PersonRef,
    status: String,
    /// Who recorded it.
    marked_by: PersonRef,
}

impl SessionAttendanceResponse {
    fn new(attendance: &SessionAttendance, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: attendance.get_id().key().to_string(),
            session: attendance.get_session().key().to_string(),
            course: attendance.get_course().key().to_string(),
            user: PersonRef::resolve(people, attendance.get_user()),
            status: attendance.get_status().as_str().to_string(),
            marked_by: PersonRef::resolve(people, attendance.get_marked_by()),
        }
    }
}

/// Resolve who a session's teacher should be: the caller when `teacher_id` is
/// omitted (or names them), otherwise the referenced user — who must exist and
/// hold the `teacher` role or higher (a student cannot teach a lesson).
pub(crate) async fn resolve_session_teacher(
    teacher_id: Option<&str>,
    caller: &User,
    db: &Database,
) -> Result<User, AppError> {
    let target = match teacher_id {
        None => return Ok(caller.clone()),
        Some(key) if key == caller.get_id().key() => return Ok(caller.clone()),
        Some(key) => UserId::from_key(key),
    };
    let Some(user) = User::read(&target, db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "session teacher does not exist",
        }));
    };
    if !user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "session teacher must hold the teacher role or higher",
        }));
    }
    Ok(user)
}

/// Load a session and its course together; a session whose course is gone
/// cannot happen given the delete cascade, so both misses are plain 404s.
async fn session_with_course(id: &str, db: &Database) -> Result<(CourseSession, Course), AppError> {
    let session = CourseSession::read(&CourseSessionId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = crate::service::course::read(db, session.get_course())
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((session, course))
}

/// Whether `user` is the session's own teacher *and* still `teacher`+ today.
///
/// The `teacher` column is a historical fact like a course's `creator` (a
/// demotion never rewrites past sessions), so teaching a session grants nothing
/// once the account falls below `teacher` — see [`can_manage_course`], which
/// carries the same floor.
fn is_live_session_teacher(session: &CourseSession, user: &User) -> bool {
    user.get_role().at_least(Role::Teacher) && session.is_teacher(user.get_id())
}

/// Who may take (or amend) a session's roll call: the session's own teacher,
/// or anyone with course-management rights (creator / manager+) — in both
/// cases only while still `teacher`+.
fn can_roll_call(session: &CourseSession, course: &Course, user: &User) -> bool {
    is_live_session_teacher(session, user) || can_manage_course(course, user)
}

// ---- sessions -------------------------------------------------------------

/// Fetch a single session by id. Visible to its course's enrolled users, the
/// session's teacher, the course's own teachers, and managers/admins.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session", body = SessionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the session teacher, and without course rights", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_session(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<SessionResponse>, AppError> {
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !is_live_session_teacher(&session, &user) && !can_view_course(&course, &user, &st.db).await?
    {
        return Err(AppError::Forbidden(
            "only enrolled users, the session teacher, the course's teachers, or a manager/admin can view this session",
        ));
    }
    let people = person_map([session.get_teacher().clone()], &st.db).await?;
    Ok(Json(SessionResponse::new(&session, &people)))
}

/// Update a session. Requires teacher+ with course-management rights. Omitted
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
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
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this session",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;

    let topic = req
        .topic
        .as_deref()
        .map(SessionTopic::try_new)
        .transpose()?;
    let teacher = match req.teacher_id {
        Some(ref key) => Some(
            resolve_session_teacher(Some(key), &user, &st.db)
                .await?
                .get_id()
                .clone(),
        ),
        None => None,
    };
    // The end this request left out is only *read* for the range check — it is
    // never written back, so a concurrent move of it survives. Pre-flight only:
    // `CourseSession::update` re-makes this check in the UPDATE's `WHERE`.
    check_time_range(
        Some(starts_at.unwrap_or_else(|| session.get_starts_at())),
        ends_at.unwrap_or_else(|| session.get_ends_at()),
    )?;

    let updated = session
        .update(teacher, topic, starts_at, ends_at, &st.db)
        .await?;
    let people = person_map([updated.get_teacher().clone()], &st.db).await?;
    Ok(Json(SessionResponse::new(&updated, &people)))
}

/// Delete a session and its roll-call rows. Requires teacher+ with
/// course-management rights.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_session(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this session",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    session.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- roll call --------------------------------------------------------------

/// Record a user's roll-call state for a session. The session's teacher or a
/// course manager marks **enrolled students** (only students attend classes);
/// marking the **session's teacher** requires manager+ (staff presence is
/// management's call, so a teacher can't mark themselves present). Students
/// never self-mark a lesson.
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
        (status = 403, description = "Not the session teacher or a course manager; or marking the teacher without manager+", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn mark_roll_call(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<MarkRollCall>,
) -> Result<Json<SessionAttendanceResponse>, AppError> {
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !can_roll_call(&session, &course, &user) {
        return Err(AppError::Forbidden(
            "only the session's teacher or a course manager can take roll call",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;

    let school = service::settings::load(&st.db).await?;
    let status = AttendanceStatus::try_new(&req.status, school.get_attendance_statuses())?;
    let target = UserId::from_key(&req.user_id);

    // Target user must exist.
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    if session.is_teacher(&target) {
        // The session teacher's own presence is recorded by management.
        if !user.get_role().at_least(Role::Manager) {
            return Err(AppError::Forbidden(
                "marking the session's teacher requires manager role or higher",
            ));
        }
    } else {
        // Everyone else on a lesson's roster is an enrolled student. Only
        // students attend classes; checking the live role keeps a stale
        // enrollment (left by a promotion) from putting staff on the roll.
        if target_user.get_role() != Role::Student {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "user_id",
                reason: "only students can be marked present in a lesson",
            }));
        }
        if Enrollment::read_for_user(session.get_course(), &target, &st.db)
            .await?
            .is_none()
        {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "user_id",
                reason: "target user is not enrolled in this course",
            }));
        }
    }

    let attendance =
        SessionAttendance::mark(&session, &target, status, user.get_id(), &st.db).await?;
    // A badge is a decoration on top of the roll call: losing one to a
    // transient database error must never fail the mark, and the next counter
    // move re-runs this and heals it. Both people the mark can credit are
    // synced — the person marked (`lessons_attended`) and the lesson's teacher
    // (`lessons_held`, on the first roll call only). The teacher's runs on every
    // mark rather than only that first one: `sync` is add-only and idempotent,
    // so the extra calls cost one record read and are what heals a first sync
    // that failed, and knowing here whether the credit landed would mean
    // widening `mark`'s return type for nothing.
    for who in [&target, session.get_teacher()] {
        if let Err(err) = badge::sync(who, &st.db).await {
            tracing::warn!("failed to sync badges for {}: {err}", who.key());
        }
    }
    let people = PersonRef::map_of(&[&target_user, &user]);
    Ok(Json(SessionAttendanceResponse::new(&attendance, &people)))
}

/// List a session's roll call, paged via `?limit=&offset=` (omit `limit` for
/// the whole roster). Same rights as taking it: the session's teacher or a
/// course manager — students see their own tallies via `GET /attendance/me`.
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
        (status = 403, description = "Not the session teacher or a course manager", body = ErrorResponse),
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
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !can_roll_call(&session, &course, &user) {
        return Err(AppError::Forbidden(
            "only the session's teacher or a course manager can list the roll call",
        ));
    }
    let (rows, total) =
        SessionAttendance::list_for_session(session.get_id(), limit, offset, &st.db).await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|a| [a.get_user().clone(), a.get_marked_by().clone()]),
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
/// session teacher or course manager for students, manager+ for a staff row
/// (any target holding teacher or higher).
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
        (status = 403, description = "Not the session teacher or a course manager; or removing a staff row without manager+", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn remove_roll_call(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !can_roll_call(&session, &course, &user) {
        return Err(AppError::Forbidden(
            "only the session's teacher or a course manager can take roll call",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    let target = UserId::from_key(&target);
    // A staff row is management's to remove, keyed on the *target's live role*
    // rather than on `is_teacher`: reassigning a session's teacher used to hand
    // the incoming teacher — an ordinary one — the outgoing teacher's staff row
    // to delete without the manager+ this guard exists to require. A target
    // whose user row is gone can only be a student's stale row (marking checks
    // the role at write time), so it stays the session teacher's to clear.
    let staff_row = User::read(&target, &st.db)
        .await?
        .is_some_and(|target_user| target_user.get_role().at_least(Role::Teacher));
    if staff_row && !user.get_role().at_least(Role::Manager) {
        return Err(AppError::Forbidden(
            "removing a staff roll-call row requires manager role or higher",
        ));
    }
    let removed = SessionAttendance::remove(session.get_id(), &target, &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
