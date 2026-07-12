use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::course::Course;
use crate::domain::course_session::{CourseSession, CourseSessionId, SessionTopic};
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::session_attendance::SessionAttendance;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::can_manage_course;
use super::{
    CurrentUser, PersonRef, RequireTeacher, SessionResponse, check_not_past, check_time_range,
    person_map, set_or_clear,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_session, update_session, delete_session))
        .routes(routes!(mark_roll_call, list_roll_call))
        .routes(routes!(remove_roll_call))
}

#[derive(Deserialize, ToSchema)]
struct UpdateSession {
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
    /// One of the accepted attendance statuses (e.g. `present`, `absent`).
    #[schema(example = "present")]
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
    let course = Course::read(session.get_course(), db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((session, course))
}

/// Who may take (or amend) a session's roll call: the session's own teacher,
/// or anyone with course-management rights (creator / manager+).
fn can_roll_call(session: &CourseSession, course: &Course, user: &User) -> bool {
    session.is_teacher(user.get_id()) || can_manage_course(course, user)
}

// ---- sessions -------------------------------------------------------------

/// Fetch a single session by id.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session", body = SessionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_session(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<SessionResponse>, AppError> {
    let session = CourseSession::read(&CourseSessionId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn update_session(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateSession>,
) -> Result<Json<SessionResponse>, AppError> {
    let (session, course) = session_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can edit this session",
        ));
    }

    let topic = match req.topic {
        Some(ref topic) => SessionTopic::try_new(topic)?,
        None => session.get_topic().clone(),
    };
    let teacher = match req.teacher_id {
        Some(ref key) => resolve_session_teacher(Some(key), &user, &st.db)
            .await?
            .get_id()
            .clone(),
        None => session.get_teacher().clone(),
    };
    // Only values this request sets are held to the no-past rule — a kept
    // `starts_at` of a lesson already underway is legitimately past.
    let starts_at = match req.starts_at {
        Some(millis) => {
            let starts_at = Timestamp::from_millis(millis);
            check_not_past("starts_at", Some(starts_at))?;
            starts_at
        }
        None => session.get_starts_at(),
    };
    // A provided value sets the field, an explicit `null` clears it, and an
    // omitted one keeps the current value.
    let ends_at = match req.ends_at {
        Some(update) => {
            let ends_at = update.map(Timestamp::from_millis);
            check_not_past("ends_at", ends_at)?;
            ends_at
        }
        None => session.get_ends_at(),
    };
    check_time_range(Some(starts_at), ends_at)?;

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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
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
            "only the course creator or a manager/admin can delete this session",
        ));
    }
    session.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- roll call --------------------------------------------------------------

/// Record a user's roll-call state for a session. The session's teacher or a
/// course manager marks **enrolled students**; marking the **session's
/// teacher** requires manager+ (staff presence is management's call, so a
/// teacher can't mark themselves present). Students never self-mark a lesson.
#[utoipa::path(
    post,
    path = "/{id}/attendance",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    request_body = MarkRollCall,
    responses(
        (status = 200, description = "Roll-call state recorded", body = SessionAttendanceResponse),
        (status = 400, description = "Invalid status, unknown user, or target not on the roster", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the session teacher or a course manager; or marking the teacher without manager+", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
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

    let status = AttendanceStatus::try_new(&req.status)?;
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
    } else if Enrollment::read_for_user(session.get_course(), &target, &st.db)
        .await?
        .is_none()
    {
        // Everyone else on a lesson's roster comes from the enrollment list.
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user is not enrolled in this course",
        }));
    }

    let attendance =
        SessionAttendance::mark(&session, &target, status, user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&target_user, &user]);
    Ok(Json(SessionAttendanceResponse::new(&attendance, &people)))
}

/// List a session's roll call.
#[utoipa::path(
    get,
    path = "/{id}/attendance",
    tag = "sessions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "Roll-call roster", body = [SessionAttendanceResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
    ),
)]
async fn list_roll_call(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<SessionAttendanceResponse>>, AppError> {
    let session = CourseSession::read(&CourseSessionId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let roster = SessionAttendance::list_for_session(session.get_id(), &st.db).await?;
    let people = person_map(
        roster
            .iter()
            .flat_map(|a| [a.get_user().clone(), a.get_marked_by().clone()]),
        &st.db,
    )
    .await?;
    Ok(Json(
        roster
            .iter()
            .map(|a| SessionAttendanceResponse::new(a, &people))
            .collect(),
    ))
}

/// Remove a user's roll-call row from a session. Same rights as marking:
/// session teacher or course manager for students, manager+ for the session
/// teacher's own row.
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
        (status = 403, description = "Not the session teacher or a course manager; or removing the teacher's row without manager+", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
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
    let target = UserId::from_key(&target);
    if session.is_teacher(&target) && !user.get_role().at_least(Role::Manager) {
        return Err(AppError::Forbidden(
            "removing the session teacher's row requires manager role or higher",
        ));
    }
    let removed = SessionAttendance::remove(session.get_id(), &target, &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
