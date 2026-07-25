use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::attendance::{Attendance, AttendanceStatus};
use crate::domain::course::{Course, CourseId};
use crate::domain::event::{
    EVENT_LOCK, Event, EventAudience, EventDescription, EventId, EventTitle,
};
use crate::domain::registration::Registration;
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireTeacher, Scheduled, WindowParams,
    check_not_past, check_time_range, paginate, person_map, set_or_clear,
};

impl Scheduled for Event {
    fn starts_at_ms(&self) -> Option<i64> {
        self.get_starts_at().map(|at| at.as_millis())
    }

    fn ends_at_ms(&self) -> Option<i64> {
        self.get_ends_at().map(|at| at.as_millis())
    }

    fn order_key(&self) -> &str {
        self.get_id().key()
    }
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_event, list_events))
        .routes(routes!(get_event, update_event, delete_event))
        .routes(routes!(mark, list_attendance))
        .routes(routes!(remove_attendance))
        .routes(routes!(roster))
        .routes(routes!(register))
        .routes(routes!(unregister))
}

/// An event's audience on the wire — who is expected to attend. Tagged by
/// `kind`; the extra field each kind needs rides alongside it.
#[derive(Serialize, Deserialize, Clone, ToSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum AudienceDto {
    /// Everybody in the school. The default when a create request omits
    /// `audience` entirely.
    School,
    /// Every user holding exactly this role — `parent`, `student`, `teacher`,
    /// `manager`, or `admin` (no implied "and above").
    Role { role: String },
    /// The students currently enrolled in this course (live — enrollment
    /// changes move people in and out).
    Course { course: String },
    /// A signup list built through `POST /events/{id}/register` — teachers
    /// place students, staff take their own seat. `capacity` caps the seats;
    /// `null` or omitted = unlimited. The list closes when the event starts.
    Registration {
        #[serde(default)]
        capacity: Option<i64>,
    },
}

impl AudienceDto {
    /// Validate the wire form into the domain audience: the role must parse,
    /// the course must exist, and a registration capacity must be positive.
    async fn into_domain(self, db: &Database) -> Result<EventAudience, AppError> {
        match self {
            AudienceDto::School => Ok(EventAudience::School),
            AudienceDto::Role { role } => Ok(EventAudience::Role {
                role: Role::try_from_str(&role)?,
            }),
            AudienceDto::Course { course } => {
                let course = CourseId::from_key(&course);
                if Course::read(&course, db).await?.is_none() {
                    return Err(AppError::Validation(ValidationError::Invalid {
                        field: "audience",
                        reason: "course does not exist",
                    }));
                }
                Ok(EventAudience::Course { course })
            }
            AudienceDto::Registration { capacity } => {
                if capacity.is_some_and(|capacity| capacity < 1) {
                    return Err(AppError::Validation(ValidationError::Invalid {
                        field: "audience",
                        reason: "capacity must be at least 1",
                    }));
                }
                Ok(EventAudience::Registration { capacity })
            }
        }
    }

    fn from_domain(audience: &EventAudience) -> Self {
        match audience {
            EventAudience::School => AudienceDto::School,
            EventAudience::Role { role } => AudienceDto::Role {
                role: role.as_str().to_string(),
            },
            EventAudience::Course { course } => AudienceDto::Course {
                course: course.key().to_string(),
            },
            EventAudience::Registration { capacity } => AudienceDto::Registration {
                capacity: *capacity,
            },
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateEvent {
    #[schema(max_length = 200, example = "Sprint demo")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// Who the event is for (its expected-attendee roster). Omit for a
    /// school-wide event.
    audience: Option<AudienceDto>,
    /// Unix-millisecond timestamps. Must not be in the past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: Option<i64>,
    ends_at: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateEvent {
    #[schema(max_length = 200)]
    title: Option<String>,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// Replaces the audience wholesale when present; omit to keep the current
    /// one. Attendance rows for people the change drops out of the roster stay
    /// stored — they just stop appearing on the roster report.
    audience: Option<AudienceDto>,
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
    /// One of the school's attendance statuses (`GET /settings`; the core
    /// four are `present`, `absent`, `late`, `excused`).
    #[schema(max_length = 50, example = "present")]
    status: String,
    /// Target user id — must be in the event's audience. Defaults to the
    /// caller when omitted.
    user_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct EventResponse {
    id: String,
    creator: String,
    title: String,
    description: String,
    audience: AudienceDto,
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
            audience: AudienceDto::from_domain(event.get_audience()),
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

/// Create an event owned by the current user. Requires the `teacher` role or
/// higher. `audience` targets it at a role, a course's enrollment, or a
/// registration list (filled via `POST /events/{id}/register`); omitted it is
/// school-wide. Everyone still sees every event — the audience is the
/// expected-attendee roster, not a visibility wall.
#[utoipa::path(
    post,
    path = "/",
    tag = "events",
    security(("session_cookie" = [])),
    request_body = CreateEvent,
    responses(
        (status = 201, description = "Event created", body = EventResponse),
        (status = 400, description = "Invalid fields, audience, time range, or times in the past", body = ErrorResponse),
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
    let audience = match req.audience {
        Some(audience) => audience.into_domain(&st.db).await?,
        None => EventAudience::School,
    };
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    check_not_past("starts_at", starts_at)?;
    check_not_past("ends_at", ends_at)?;
    check_time_range(starts_at, ends_at)?;
    let event = Event::create(
        user.get_id(),
        title,
        description,
        audience,
        starts_at,
        ends_at,
        &st.db,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(EventResponse::new(&event))))
}

/// List all events, newest first. Paged via `?limit=&offset=` (omit `limit`
/// for every event); returns a `{items, total, limit, offset}` envelope.
///
/// The optional `?starts_after=&ends_after=` schedule window (unix
/// milliseconds) narrows the list to upcoming/unfinished events and flips the
/// order to ascending by schedule, so `?ends_after=<now>&limit=20` returns the
/// twenty *soonest* events rather than the twenty newest-created. Events with
/// no schedule are excluded by either parameter.
#[utoipa::path(
    get,
    path = "/",
    tag = "events",
    security(("session_cookie" = [])),
    params(WindowParams, PageParams),
    responses(
        (status = 200, description = "A page of events (all of them when unpaged)", body = Page<EventResponse>),
        (status = 400, description = "Invalid window, limit, or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_events(
    State(st): State<AppState>,
    _user: CurrentUser,
    Query(window): Query<WindowParams>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<EventResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let events = window.apply(Event::list_all(&st.db).await?)?;
    let total = events.len() as i64;
    let items = paginate(&events, limit, offset)
        .iter()
        .map(EventResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
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
/// explicit `null` clears `starts_at`/`ends_at`. A provided `audience` replaces
/// the current one wholesale — attendance and signup rows for people it drops
/// stay stored but leave the roster report (signups resurface if the event is
/// switched back to the registration kind).
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    request_body = UpdateEvent,
    responses(
        (status = 200, description = "Updated event", body = EventResponse),
        (status = 400, description = "Invalid fields, audience, time range, or times in the past", body = ErrorResponse),
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
    // The schedule columns are nullable, so they keep the outer/inner
    // distinction: a provided value sets the field, an explicit `null` clears
    // it, and an omitted one is left alone.
    let starts_at = req
        .starts_at
        .map(|update| update.map(Timestamp::from_millis));
    let ends_at = req.ends_at.map(|update| update.map(Timestamp::from_millis));
    // Only the range check needs the stored row, and only it can race: it
    // validates an arriving end against the other end as stored, so the read,
    // the check and the write are held together under [`EVENT_LOCK`]. A PATCH
    // that moves neither end pays nothing.
    let _guard = match (starts_at, ends_at) {
        (None, None) => None,
        _ => Some(EVENT_LOCK.lock().await),
    };

    let event = Event::read(&EventId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&event, &user) {
        return Err(AppError::Forbidden(
            "only the creator or a manager/admin can edit this event",
        ));
    }

    // Only what the request carried: an omitted field stays `None` and is
    // never written, so a concurrent PATCH of another field survives. (A JSON
    // `null` deserializes to `None` for title/description/audience too —
    // those columns are not nullable, so "omitted" and "null" both mean
    // "keep".)
    let title = req.title.as_deref().map(EventTitle::try_new).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(EventDescription::try_new)
        .transpose()?;
    let audience = match req.audience {
        Some(audience) => Some(audience.into_domain(&st.db).await?),
        None => None,
    };
    // Only set schedule values are held to the no-past rule — a kept time of
    // an event already underway may be past.
    if let Some(starts_at) = starts_at {
        check_not_past("starts_at", starts_at)?;
    }
    if let Some(ends_at) = ends_at {
        check_not_past("ends_at", ends_at)?;
    }
    // The range CHECK needs both ends: whichever the request omitted comes
    // from the stored row. Read for the check only — the omitted side is
    // never written back.
    check_time_range(
        starts_at.unwrap_or_else(|| event.get_starts_at()),
        ends_at.unwrap_or_else(|| event.get_ends_at()),
    )?;

    let updated = event
        .update(title, description, audience, starts_at, ends_at, &st.db)
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

/// Mark attendance for a user on an event (defaults to the caller). Taking
/// attendance is a teacher+ action — students never mark, not even themselves —
/// and the target must be in the event's audience.
#[utoipa::path(
    post,
    path = "/{id}/attendance",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    request_body = MarkAttendance,
    responses(
        (status = 200, description = "Attendance recorded", body = AttendanceResponse),
        (status = 400, description = "Invalid status, unknown user, or target outside the event's audience", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
    ),
)]
async fn mark(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<MarkAttendance>,
) -> Result<Json<AttendanceResponse>, AppError> {
    let event_id = EventId::from_key(&id);
    let event = Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let school = Settings::load(&st.db).await?;
    let status = AttendanceStatus::try_new(&req.status, school.get_attendance_statuses())?;
    let target = match req.user_id {
        Some(ref key) => UserId::from_key(key),
        None => user.get_id().clone(),
    };

    // Target user must exist.
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    // Only expected attendees can be marked. The marker needn't be in the
    // audience — a teacher takes roll of a student-targeted event.
    if !event
        .get_audience()
        .includes(event.get_id(), &target_user, &st.db)
        .await?
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user is not in this event's audience",
        }));
    }

    let attendance = Attendance::mark(&event_id, &target, status, user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&target_user, &user]);
    Ok(Json(AttendanceResponse::new(&attendance, &people)))
}

/// List the attendance roster for an event, paged via `?limit=&offset=` (omit
/// `limit` for the whole roster). Requires teacher+ — students see their own
/// tallies via `GET /attendance/me`. Returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/{id}/attendance",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id"), PageParams),
    responses(
        (status = 200, description = "A page of the attendance roster (all of it when unpaged)", body = Page<AttendanceResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
    ),
)]
async fn list_attendance(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<AttendanceResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let event_id = EventId::from_key(&id);
    // Event must exist — a missing event is a 404, not an empty roster.
    Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let roster = Attendance::list_for_event(&event_id, &st.db).await?;
    let total = roster.len() as i64;
    // Join people onto the page alone — the lookup shrinks with the window.
    let rows = paginate(&roster, limit, offset);
    let people = person_map(
        rows.iter()
            .flat_map(|a| [a.get_user().clone(), a.get_marked_by().clone()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|a| AttendanceResponse::new(a, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
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

// ---- roster ---------------------------------------------------------------

/// One expected attendee on the roster report: who they are and how (whether)
/// they were marked.
#[derive(Serialize, ToSchema)]
struct RosterEntry {
    user: PersonRef,
    /// The recorded attendance status; `null` = expected but never marked —
    /// the "missed" signal once the event is over.
    status: Option<String>,
    /// Who recorded it; `null` while unmarked.
    marked_by: Option<PersonRef>,
}

/// The event's expected-attendee roster joined with its attendance marks — the
/// who-came/who-missed report. Resolved live from the audience (today's role
/// holders, current enrollment, the current signup list), so it always reflects
/// the present roster;
/// attendance rows for people no longer in the audience are omitted here (they
/// remain in `GET /events/{id}/attendance`). Requires teacher+. Paged via
/// `?limit=&offset=` (omit `limit` for the whole roster).
#[utoipa::path(
    get,
    path = "/{id}/roster",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id"), PageParams),
    responses(
        (status = 200, description = "A page of the expected-attendee roster (all of it when unpaged)", body = Page<RosterEntry>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
    ),
)]
async fn roster(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<RosterEntry>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let event_id = EventId::from_key(&id);
    let event = Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let mut members = event.get_audience().members(event.get_id(), &st.db).await?;
    // ULID keys sort by creation instant — a stable order keeps pages coherent.
    members.sort_by(|a, b| a.key().cmp(b.key()));
    let marks = Attendance::list_for_event(&event_id, &st.db).await?;
    let by_user: HashMap<&str, &Attendance> = marks
        .iter()
        .map(|attendance| (attendance.get_user().key(), attendance))
        .collect();

    let total = members.len() as i64;
    // Join people onto the page alone — the lookup shrinks with the window.
    let window = paginate(&members, limit, offset);
    let ids = window.iter().cloned().chain(window.iter().filter_map(|m| {
        by_user
            .get(m.key())
            .map(|attendance| attendance.get_marked_by().clone())
    }));
    let people = person_map(ids, &st.db).await?;
    let items = window
        .iter()
        .map(|member| {
            let mark = by_user.get(member.key());
            RosterEntry {
                user: PersonRef::resolve(&people, member),
                status: mark.map(|attendance| attendance.get_status().as_str().to_string()),
                marked_by: mark
                    .map(|attendance| PersonRef::resolve(&people, attendance.get_marked_by())),
            }
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- registration ---------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct RegisterUser {
    /// Target user id — must name a student (staff take seats only for
    /// themselves). Defaults to the caller when omitted.
    user_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct RegistrationResponse {
    event: String,
    /// Who holds the seat.
    user: PersonRef,
    /// Who placed them on the list.
    registered_by: PersonRef,
}

/// Put a user on a registration-audience event's signup list. Requires
/// teacher+. `user_id` must name a student — students are placed by staff and
/// never register themselves; omit it to take a seat yourself (staff
/// self-serve, so registering another teacher/manager is refused). Registering
/// the same person twice is a no-op returning the existing seat. The list
/// closes when the event starts (or its ends_at-only deadline passes) and
/// refuses to grow past `capacity`.
#[utoipa::path(
    post,
    path = "/{id}/register",
    tag = "events",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Event id")),
    request_body = RegisterUser,
    responses(
        (status = 200, description = "Seat taken (or already held)", body = RegistrationResponse),
        (status = 400, description = "Not a registration event, or the target is unknown", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or the target is another staff member", body = ErrorResponse),
        (status = 404, description = "Event not found", body = ErrorResponse),
        (status = 409, description = "The event is full, or it already started or ended", body = ErrorResponse),
    ),
)]
async fn register(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<RegisterUser>,
) -> Result<Json<RegistrationResponse>, AppError> {
    let event_id = EventId::from_key(&id);
    let event = Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // Fast-fail gate; the authoritative re-check runs inside
    // `Registration::register` under its lock.
    event.registration_capacity()?;

    let target = match req.user_id {
        Some(ref key) => UserId::from_key(key),
        None => user.get_id().clone(),
    };
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    // Students are placed by staff; staff hold only their own seat.
    if &target != user.get_id() && target_user.get_role() != Role::Student {
        return Err(AppError::Forbidden(
            "staff register themselves — only students can be registered for",
        ));
    }

    let registration = Registration::register(&event_id, &target, user.get_id(), &st.db).await?;
    // Resolve from the row, not the request: a re-register returns the
    // existing seat, whose registered_by is the *original* placer — someone
    // the {target, caller} pair may not contain.
    let people = person_map(
        [
            registration.get_user().clone(),
            registration.get_registered_by().clone(),
        ],
        &st.db,
    )
    .await?;
    Ok(Json(RegistrationResponse {
        event: registration.get_event().key().to_string(),
        user: PersonRef::resolve(&people, registration.get_user()),
        registered_by: PersonRef::resolve(&people, registration.get_registered_by()),
    }))
}

/// Take a user off the signup list — the register rules mirrored: teacher+,
/// students' seats or your own (another staff member's seat only if their
/// account no longer exists), and only while the list is open (the event
/// hasn't started or, ends_at-only, passed). Attendance already marked stays
/// recorded.
#[utoipa::path(
    delete,
    path = "/{id}/register/{user}",
    tag = "events",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Event id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Seat freed"),
        (status = 400, description = "Not a registration event", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or the target is another staff member", body = ErrorResponse),
        (status = 404, description = "Event not found, or the user holds no seat", body = ErrorResponse),
        (status = 409, description = "The event already started or ended", body = ErrorResponse),
    ),
)]
async fn unregister(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let event_id = EventId::from_key(&id);
    let event = Event::read(&event_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    event.registration_capacity()?;

    let target = UserId::from_key(&target);
    // A deleted account's leftover seat is fair game for any teacher+; a
    // living staff member's seat is theirs alone.
    if &target != user.get_id()
        && let Some(target_user) = User::read(&target, &st.db).await?
        && target_user.get_role() != Role::Student
    {
        return Err(AppError::Forbidden(
            "staff unregister themselves — only students' seats can be freed",
        ));
    }

    if Registration::remove(&event_id, &target, &st.db)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
