use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::attendance::{Attendance, AttendanceStatus};
use crate::domain::event::{Event, EventDescription, EventId, EventTitle};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{
    CurrentUser, PersonRef, RequireTeacher, check_not_past, check_time_range, person_map,
    set_or_clear,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_event, list_events))
        .routes(routes!(get_event, update_event, delete_event))
        .routes(routes!(mark, list_attendance))
        .routes(routes!(remove_attendance))
}

#[derive(Deserialize, ToSchema)]
struct CreateEvent {
    #[schema(example = "Sprint demo")]
    title: String,
    description: Option<String>,
    /// Unix-millisecond timestamps. Must not be in the past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: Option<i64>,
    ends_at: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateEvent {
    title: Option<String>,
    description: Option<String>,
    /// Unix-millisecond timestamp. Omit to keep the current value; send `null`
    /// to clear it. A newly set value must not be in the past.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    starts_at: Option<Option<i64>>,
    /// Unix-millisecond timestamp. Omit to keep the current value; send `null`
    /// to clear it. A newly set value must not be in the past.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    ends_at: Option<Option<i64>>,
}

#[derive(Deserialize, ToSchema)]
struct MarkAttendance {
    /// One of the accepted attendance statuses (e.g. `present`, `absent`).
    #[schema(example = "present")]
    status: String,
    /// Target user id. Defaults to the caller when omitted.
    user_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct EventResponse {
    id: String,
    creator: String,
    title: String,
    description: String,
    starts_at: Option<i64>,
    ends_at: Option<i64>,
}

impl EventResponse {
    fn new(event: &Event) -> Self {
        Self {
            id: event.get_id().key().to_string(),
            creator: event.get_creator().key().to_string(),
            title: event.get_title().as_str().to_string(),
            description: event.get_description().as_str().to_string(),
            starts_at: event.get_starts_at().map(|t| t.as_millis()),
            ends_at: event.get_ends_at().map(|t| t.as_millis()),
        }
    }
}

/// Who may edit/delete a specific event: its creator, or anyone `manager` and
/// above (who can manage any event regardless of ownership). Callers have
/// already cleared the `teacher` bar via the `RequireTeacher` extractor.
fn can_manage(event: &Event, user: &User) -> bool {
    event.is_creator(user.get_id()) || user.get_role().at_least(Role::Manager)
}

#[derive(Serialize, ToSchema)]
struct AttendanceResponse {
    id: String,
    event: String,
    /// Whose attendance this row records.
    user: PersonRef,
    status: String,
    /// Who recorded it.
    marked_by: PersonRef,
}

impl AttendanceResponse {
    fn new(attendance: &Attendance, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: attendance.get_id().key().to_string(),
            event: attendance.get_event().key().to_string(),
            user: PersonRef::resolve(people, attendance.get_user()),
            status: attendance.get_status().as_str().to_string(),
            marked_by: PersonRef::resolve(people, attendance.get_marked_by()),
        }
    }
}

// ---- events -------------------------------------------------------------

/// Create an event owned by the current user. Requires the `teacher` role or higher.
#[utoipa::path(
    post,
    path = "/",
    tag = "events",
    security(("session_cookie" = [])),
    request_body = CreateEvent,
    responses(
        (status = 201, description = "Event created", body = EventResponse),
        (status = 400, description = "Invalid fields, time range, or times in the past", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn create_event(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateEvent>,
) -> Result<(StatusCode, Json<EventResponse>), AppError> {
    let title = EventTitle::try_new(&req.title)?;
    let description = EventDescription::try_new(&req.description.unwrap_or_default())?;
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    check_not_past("starts_at", starts_at)?;
    check_not_past("ends_at", ends_at)?;
    check_time_range(starts_at, ends_at)?;
    let event = Event::create(
        user.get_id(),
        title,
        description,
        starts_at,
        ends_at,
        &st.db,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(EventResponse::new(&event))))
}

/// List all events.
#[utoipa::path(
    get,
    path = "/",
    tag = "events",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "All events", body = [EventResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_events(
    State(st): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<EventResponse>>, AppError> {
    let events = Event::list_all(&st.db).await?;
    Ok(Json(events.iter().map(EventResponse::new).collect()))
}

/// Fetch a single event by id.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    responses(
        (status = 200, description = "The event", body = EventResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_event(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<EventResponse>, AppError> {
    let event = Event::read(&EventId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(EventResponse::new(&event)))
}

/// Update an event. Requires teacher+; the creator may edit their own event and
/// managers/admins may edit anyone's. Omitted fields keep their value; an
/// explicit `null` clears `starts_at`/`ends_at`.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    request_body = UpdateEvent,
    responses(
        (status = 200, description = "Updated event", body = EventResponse),
        (status = 400, description = "Invalid fields, time range, or times in the past", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn update_event(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateEvent>,
) -> Result<Json<EventResponse>, AppError> {
    let event = Event::read(&EventId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&event, &user) {
        return Err(AppError::Forbidden(
            "only the creator or a manager/admin can edit this event",
        ));
    }

    let title = match req.title {
        Some(ref title) => EventTitle::try_new(title)?,
        None => event.get_title().clone(),
    };
    let description = match req.description {
        Some(ref description) => EventDescription::try_new(description)?,
        None => event.get_description().clone(),
    };
    // A provided value sets the field, an explicit `null` clears it, and an
    // omitted one keeps the current value. Only set values are held to the
    // no-past rule — a kept time of an event already underway may be past.
    let starts_at = match req.starts_at {
        Some(update) => {
            let starts_at = update.map(Timestamp::from_millis);
            check_not_past("starts_at", starts_at)?;
            starts_at
        }
        None => event.get_starts_at(),
    };
    let ends_at = match req.ends_at {
        Some(update) => {
            let ends_at = update.map(Timestamp::from_millis);
            check_not_past("ends_at", ends_at)?;
            ends_at
        }
        None => event.get_ends_at(),
    };
    check_time_range(starts_at, ends_at)?;

    let updated = event
        .update(title, description, starts_at, ends_at, &st.db)
        .await?;
    Ok(Json(EventResponse::new(&updated)))
}

/// Delete an event. Requires teacher+; the creator may delete their own event
/// and managers/admins may delete anyone's.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_event(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let event = Event::read(&EventId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&event, &user) {
        return Err(AppError::Forbidden(
            "only the creator or a manager/admin can delete this event",
        ));
    }
    event.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- attendance ---------------------------------------------------------

/// Mark attendance for a user on an event (defaults to the caller). Anyone may
/// mark their own attendance; marking someone else requires the `teacher` role
/// or higher.
#[utoipa::path(
    post,
    path = "/{id}/attendance",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    request_body = MarkAttendance,
    responses(
        (status = 200, description = "Attendance recorded", body = AttendanceResponse),
        (status = 400, description = "Invalid status or unknown user", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Marking another user requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
    ),
)]
async fn mark(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<MarkAttendance>,
) -> Result<Json<AttendanceResponse>, AppError> {
    let event_id = EventId::from_key(&id);
    // Event must exist.
    Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let status = AttendanceStatus::try_new(&req.status)?;
    let target = match req.user_id {
        Some(ref key) => UserId::from_key(key),
        None => user.get_id().clone(),
    };

    // Marking anyone other than yourself is a teacher+ action.
    if &target != user.get_id() && !user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Forbidden(
            "only teachers can mark attendance for other users",
        ));
    }

    // Target user must exist.
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    let attendance = Attendance::mark(&event_id, &target, status, user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&target_user, &user]);
    Ok(Json(AttendanceResponse::new(&attendance, &people)))
}

/// List the attendance roster for an event.
#[utoipa::path(
    get,
    path = "/{id}/attendance",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    responses(
        (status = 200, description = "Attendance roster", body = [AttendanceResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
    ),
)]
async fn list_attendance(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<AttendanceResponse>>, AppError> {
    let event_id = EventId::from_key(&id);
    // Event must exist — a missing event is a 404, not an empty roster.
    Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let roster = Attendance::list_for_event(&event_id, &st.db).await?;
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
            .map(|a| AttendanceResponse::new(a, &people))
            .collect(),
    ))
}

/// Remove a user's attendance record from an event. Requires teacher+.
#[utoipa::path(
    delete,
    path = "/{id}/attendance/{user}",
    tag = "events",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Event id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn remove_attendance(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let removed =
        Attendance::remove(&EventId::from_key(&id), &UserId::from_key(&target), &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
