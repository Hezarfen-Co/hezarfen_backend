use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::appointment::{Appointment, AppointmentId, AppointmentReason};
use crate::domain::appointment_slot::{AppointmentSlot, AppointmentSlotId, SlotNote, SlotSeries};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireTeacher, check_not_past, check_time_range,
    paginate, person_map,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(publish_slots, list_slots))
        .routes(routes!(delete_slot))
        .routes(routes!(delete_slot_series))
        .routes(routes!(book, list_appointments))
        .routes(routes!(approve))
        .routes(routes!(reject))
        .routes(routes!(cancel))
        .routes(routes!(reschedule))
        .routes(routes!(accept_reschedule))
        .routes(routes!(decline_reschedule))
}

#[derive(Deserialize, ToSchema)]
struct PublishSlots {
    /// Unix-millisecond timestamps. Must not be in the past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: i64,
    ends_at: i64,
    /// Free-text hint shown to requesters ("office hours", "veli görüşmesi").
    note: Option<String>,
    /// Repeat the same window every week up to and including `until`.
    #[serde(default)]
    repeat_weekly: bool,
    /// Required with `repeat_weekly`, ignored without it. Unix-milliseconds,
    /// not in the past; at most 52 occurrences may be expanded.
    until: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
struct BookAppointment {
    /// The slot to take, from `GET /appointments/slots`.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    slot: String,
    /// Why you want the meeting — required, the teacher decides on it.
    #[schema(example = "Ders notlarını konuşmak istiyorum")]
    reason: String,
}

#[derive(Deserialize, ToSchema)]
struct CancelRequest {
    /// Optional free-text reason. Blank or absent records no reason; over-long
    /// (past `MAX_APPOINTMENT_REASON_LEN`) answers `400`.
    #[schema(example = "Rahatsızlandım, katılamayacağım")]
    reason: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct RejectRequest {
    /// Optional free-text reason. Blank or absent records no reason; over-long
    /// (past `MAX_APPOINTMENT_REASON_LEN`) answers `400`.
    #[schema(example = "Bu saatte müsait değilim")]
    reason: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct Reschedule {
    /// Unix-millisecond timestamps. Must not be in the past.
    #[schema(example = 1_900_000_000_000_i64)]
    starts_at: i64,
    ends_at: i64,
}

#[derive(Serialize, ToSchema)]
struct SlotResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    /// Whose calendar this is.
    teacher: PersonRef,
    /// Window open/close, UTC unix-milliseconds. Half-open: back-to-back slots
    /// (10:00–10:30, 10:30–11:00) do not collide.
    starts_at: i64,
    ends_at: i64,
    note: Option<String>,
    /// The recurring publish this occurrence came from; `null` for a one-off.
    /// `DELETE /appointments/slots/series/{series}` drops the whole group.
    series: Option<String>,
    created_at: i64,
}

impl SlotResponse {
    fn new(slot: &AppointmentSlot, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: slot.get_id().key().to_string(),
            teacher: PersonRef::resolve(people, slot.get_teacher()),
            starts_at: slot.get_starts_at().as_millis(),
            ends_at: slot.get_ends_at().as_millis(),
            note: slot.get_note().map(|note| note.as_str().to_string()),
            series: slot.get_series().map(|series| series.as_str().to_string()),
            created_at: slot.get_created_at().as_millis(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct AppointmentResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    /// The slot this booking sits on.
    slot: String,
    /// Whose slot it is; `null` only if that slot has since vanished.
    teacher: Option<PersonRef>,
    /// Who asked for the meeting (a student or a parent).
    requester: PersonRef,
    /// `pending`, `approved`, `rejected`, or `cancelled`. Only the first two
    /// hold the slot — rejecting or cancelling frees it for someone else.
    #[schema(example = "pending")]
    status: String,
    reason: String,
    /// When the meeting actually happens, UTC unix-milliseconds: the accepted
    /// counter-proposal when there is one, the slot's own window otherwise.
    starts_at: Option<i64>,
    ends_at: Option<i64>,
    /// A standing counter-proposal awaiting the requester's answer.
    proposed_starts_at: Option<i64>,
    proposed_ends_at: Option<i64>,
    proposed_by: Option<PersonRef>,
    /// Who approved or rejected it; `null` while pending.
    decided_by: Option<PersonRef>,
    /// Who called it off — the requester, or a requester declining a
    /// counter-proposal; `null` unless `status` is `cancelled`.
    cancelled_by: Option<PersonRef>,
    /// Optional free-text reason given when cancelling; `null` when none was
    /// (a blank one records nothing). Bookings are only ever rendered to the
    /// requester and to the slot's teacher, so this audit trail goes no wider.
    cancel_reason: Option<String>,
    /// Optional free-text reason given when rejecting; `null` when none was.
    /// The rejecter is on `decided_by`. Same audience as `cancel_reason`.
    reject_reason: Option<String>,
    created_at: i64,
}

impl AppointmentResponse {
    fn new(
        appointment: &Appointment,
        slot: Option<&AppointmentSlot>,
        people: &HashMap<String, PersonRef>,
    ) -> Self {
        let window = slot.map(|slot| appointment.window(slot));
        Self {
            id: appointment.get_id().key().to_string(),
            slot: appointment.get_slot().key().to_string(),
            teacher: slot.map(|slot| PersonRef::resolve(people, slot.get_teacher())),
            requester: PersonRef::resolve(people, appointment.get_requester()),
            status: appointment.get_status().as_str().to_string(),
            reason: appointment.get_reason().as_str().to_string(),
            starts_at: window.map(|(starts_at, _)| starts_at.as_millis()),
            ends_at: window.map(|(_, ends_at)| ends_at.as_millis()),
            proposed_starts_at: appointment.get_proposed_starts_at().map(|t| t.as_millis()),
            proposed_ends_at: appointment.get_proposed_ends_at().map(|t| t.as_millis()),
            proposed_by: appointment
                .get_proposed_by()
                .map(|id| PersonRef::resolve(people, id)),
            decided_by: appointment
                .get_decided_by()
                .map(|id| PersonRef::resolve(people, id)),
            cancelled_by: appointment
                .get_cancelled_by()
                .map(|id| PersonRef::resolve(people, id)),
            cancel_reason: appointment
                .get_cancel_reason()
                .map(|reason| reason.as_str().to_string()),
            reject_reason: appointment
                .get_reject_reason()
                .map(|reason| reason.as_str().to_string()),
            created_at: appointment.get_created_at().as_millis(),
        }
    }
}

/// Join slots and people onto a run of bookings. The slot read is per row, so
/// callers pass the page they are about to render, not the whole list.
async fn appointment_responses(
    rows: &[Appointment],
    db: &Database,
) -> Result<Vec<AppointmentResponse>, AppError> {
    let mut slots = Vec::with_capacity(rows.len());
    for row in rows {
        slots.push(AppointmentSlot::read(row.get_slot(), db).await?);
    }
    // Collected eagerly rather than as a lazy iterator: a borrowing closure
    // held across the `person_map` await makes the handler's future
    // higher-ranked, which axum then refuses as not `Send` enough.
    let mut ids = Vec::with_capacity(rows.len() * 2);
    for (row, slot) in rows.iter().zip(&slots) {
        ids.push(row.get_requester().clone());
        ids.extend(slot.as_ref().map(|slot| slot.get_teacher().clone()));
        ids.extend(row.get_proposed_by().cloned());
        ids.extend(row.get_decided_by().cloned());
        ids.extend(row.get_cancelled_by().cloned());
    }
    let people = person_map(ids, db).await?;
    Ok(rows
        .iter()
        .zip(&slots)
        .map(|(row, slot)| AppointmentResponse::new(row, slot.as_ref(), &people))
        .collect())
}

/// The single-row form of [`appointment_responses`] — one rendering path, so a
/// mutation's response can never drift from the list's.
async fn one_appointment(
    appointment: Appointment,
    db: &Database,
) -> Result<Json<AppointmentResponse>, AppError> {
    appointment_responses(std::slice::from_ref(&appointment), db)
        .await?
        .pop()
        .map(Json)
        .ok_or_else(|| AppError::Internal("failed to render the appointment".into()))
}

/// Who may act on a slot (and on the bookings sitting on it): the teacher who
/// published it, or anyone `manager` and above. This asks about *ownership*
/// only — it does not re-check the role bar, and `cancel` reaches it from a
/// plain `CurrentUser` handler. So a slot owner demoted to student or parent
/// still passes here and can cancel bookings on their now-inert slot; that is
/// deliberate — the slot is unbookable anyway (`book` refuses it) and calling
/// off a meeting they can no longer hold is the right outcome.
fn can_manage(slot: &AppointmentSlot, user: &User) -> bool {
    slot.get_teacher() == user.get_id() || user.get_role().at_least(Role::Manager)
}

/// The booking plus the slot it sits on, insisting the caller may decide it.
async fn for_decision(
    id: &AppointmentId,
    user: &User,
    db: &Database,
) -> Result<Appointment, AppError> {
    let appointment = Appointment::read(id, db).await?.ok_or(AppError::NotFound)?;
    let slot = AppointmentSlot::read(appointment.get_slot(), db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&slot, user) {
        return Err(AppError::Forbidden(
            "only the slot's teacher or a manager/admin can decide this appointment",
        ));
    }
    Ok(appointment)
}

// ---- slots ----------------------------------------------------------------

/// Publish availability. Requires the `teacher` role or higher; the slot lands
/// on the caller's own calendar. With `repeat_weekly` the same window is
/// expanded into one row per week up to and including `until` (at most 52),
/// all sharing a `series` id — each occurrence is then independently bookable
/// and independently deletable. The response is always an array: one element
/// for a one-off publish, one per occurrence for a weekly one.
///
/// A window may not overlap another the *same teacher* has already published
/// (`409`). The comparison is half-open, so 10:00–10:30 and 10:30–11:00 are two
/// slots and not a collision — that is how an hour is carved into back-to-back
/// slots. A weekly publish is all-or-nothing: it is validated in full (against
/// stored slots *and* against its own earlier occurrences, which a window longer
/// than a week collides with) before a single row is written, so a mid-series
/// collision leaves no stray weeks behind.
#[utoipa::path(
    post,
    path = "/slots",
    tag = "appointments",
    security(("session_cookie" = [])),
    request_body = PublishSlots,
    responses(
        (status = 201, description = "Slots published", body = Vec<SlotResponse>),
        (status = 400, description = "Invalid note, time range, times in the past, `until` missing with `repeat_weekly`, more than 52 occurrences, or a window too far ahead to shift by a week", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 409, description = "The window overlaps one the caller has already published, or (weekly) two occurrences overlap each other — the whole publish is refused, nothing is written", body = ErrorResponse),
    ),
)]
async fn publish_slots(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<PublishSlots>,
) -> Result<(StatusCode, Json<Vec<SlotResponse>>), AppError> {
    let note = match req.note {
        Some(ref note) => Some(SlotNote::try_new(note)?),
        None => None,
    };
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = Timestamp::from_millis(req.ends_at);
    check_not_past("starts_at", Some(starts_at))?;
    check_not_past("ends_at", Some(ends_at))?;
    check_time_range(Some(starts_at), Some(ends_at))?;

    let slots = if req.repeat_weekly {
        let until = req
            .until
            .map(Timestamp::from_millis)
            .ok_or(AppError::Validation(ValidationError::Invalid {
                field: "until",
                reason: "is required with repeat_weekly",
            }))?;
        check_not_past("until", Some(until))?;
        AppointmentSlot::publish_weekly(user.get_id(), starts_at, ends_at, note, until, &st.db)
            .await?
    } else {
        vec![AppointmentSlot::create(user.get_id(), starts_at, ends_at, note, &st.db).await?]
    };

    let people = PersonRef::map_of(&[&user]);
    let items = slots
        .iter()
        .map(|slot| SlotResponse::new(slot, &people))
        .collect();
    Ok((StatusCode::CREATED, Json(items)))
}

/// List slots, earliest first. Teacher+ see their own calendar, past occurrences
/// included; everyone else sees every slot still open in the future — the
/// bookable calendar. A slot whose teacher has since been demoted is left out
/// (and refused at book time). Whether a slot is already taken is not carried
/// here: booking a taken one answers `409`. Paged via `?limit=&offset=` (omit
/// `limit` for every slot); returns a `{items, total, limit, offset}` envelope.
// ponytail: occupancy would be one query per slot with today's domain API; a
// batch "live bookings for these slots" read would let the list carry it.
#[utoipa::path(
    get,
    path = "/slots",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of slots (all of them when unpaged)", body = Page<SlotResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_slots(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SlotResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (slots, people) = if user.get_role().at_least(Role::Teacher) {
        (
            AppointmentSlot::list_for_teacher(user.get_id(), &st.db).await?,
            PersonRef::map_of(&[&user]),
        )
    } else {
        let mut slots = AppointmentSlot::list_upcoming(Timestamp::now(), &st.db).await?;
        // The teachers' live rows, not the slots' word for it: a demotion
        // leaves the calendar behind, and an inert slot must not be offered.
        // Keying the person map off that same read makes the filter free.
        let mut seen = HashSet::new();
        let teachers: Vec<UserId> = slots
            .iter()
            .map(AppointmentSlot::get_teacher)
            .filter(|teacher| seen.insert(teacher.key().to_string()))
            .cloned()
            .collect();
        let people: HashMap<String, PersonRef> = User::list_by_ids(&teachers, &st.db)
            .await?
            .iter()
            .filter(|teacher| teacher.get_role().at_least(Role::Teacher))
            .map(|teacher| (teacher.get_id().key().to_string(), PersonRef::new(teacher)))
            .collect();
        slots.retain(|slot| people.contains_key(slot.get_teacher().key()));
        (slots, people)
    };
    let total = slots.len() as i64;
    let items = paginate(&slots, limit, offset)
        .iter()
        .map(|slot| SlotResponse::new(slot, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Withdraw one slot. Requires teacher+; the publishing teacher may drop their
/// own and managers/admins anyone's. A slot carrying a pending or approved
/// booking is refused (`409`) — reject or cancel that booking first. Settled
/// bookings (rejected/cancelled) go with the slot.
#[utoipa::path(
    delete,
    path = "/slots/{id}",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Slot id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the slot's teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The slot has a pending or approved booking", body = ErrorResponse),
    ),
)]
async fn delete_slot(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let slot = AppointmentSlot::read(&AppointmentSlotId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&slot, &user) {
        return Err(AppError::Forbidden(
            "only the slot's teacher or a manager/admin can delete it",
        ));
    }
    slot.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Withdraw a whole recurring publish. Same rights as the single delete, and
/// all-or-nothing: if *any* occurrence still carries a pending or approved
/// booking the entire series is refused (`409`), so the person waiting is dealt
/// with rather than silently left on a stray week.
#[utoipa::path(
    delete,
    path = "/slots/series/{series}",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("series" = String, Path, description = "Series id, from a slot's `series` field")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the series' teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such series", body = ErrorResponse),
        (status = 409, description = "Some occurrence has a pending or approved booking", body = ErrorResponse),
    ),
)]
async fn delete_slot_series(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(series): Path<String>,
) -> Result<StatusCode, AppError> {
    let series = SlotSeries::from_key(&series);
    // Every occurrence of one publish carries the same teacher, so the first
    // row answers the ownership question for all of them.
    let slot = AppointmentSlot::list_for_series(&series, &st.db)
        .await?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)?;
    if !can_manage(&slot, &user) {
        return Err(AppError::Forbidden(
            "only the slot's teacher or a manager/admin can delete it",
        ));
    }
    AppointmentSlot::delete_series(&series, &st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- bookings -------------------------------------------------------------

/// Ask for a meeting on a published slot. Students and parents only — a parent
/// books for *themselves* (this is the parent-teacher conference), never on
/// behalf of a child, and staff arrange between themselves off this API. The
/// booking lands `pending`: publishing availability is not consent to a
/// particular person and topic. Refused (`409`) when the slot's window has already
/// opened, when it is already taken, when the requester is already committed at
/// that time, or when the slot's teacher no longer holds a teaching role.
#[utoipa::path(
    post,
    path = "/",
    tag = "appointments",
    security(("session_cookie" = [])),
    request_body = BookAppointment,
    responses(
        (status = 201, description = "Booking requested", body = AppointmentResponse),
        (status = 400, description = "Missing or over-long reason", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Only students and parents book appointments", body = ErrorResponse),
        (status = 404, description = "Slot not found", body = ErrorResponse),
        (status = 409, description = "The slot has started or is taken, the requester is busy, or its teacher is no longer staff", body = ErrorResponse),
    ),
)]
async fn book(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<BookAppointment>,
) -> Result<(StatusCode, Json<AppointmentResponse>), AppError> {
    if !matches!(user.get_role(), Role::Student | Role::Parent) {
        return Err(AppError::Forbidden(
            "only students and parents can book an appointment",
        ));
    }
    let slot_id = AppointmentSlotId::from_key(&req.slot);
    let slot = AppointmentSlot::read(&slot_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // The slot row is not the grant: a teacher demoted since publishing keeps
    // their rows, and those rows must be inert. Their live role decides.
    if !User::read(slot.get_teacher(), &st.db)
        .await?
        .is_some_and(|teacher| teacher.get_role().at_least(Role::Teacher))
    {
        return Err(AppError::Conflict(
            "the slot's teacher no longer holds a teaching role",
        ));
    }
    let reason = AppointmentReason::try_new(&req.reason)?;
    let appointment = Appointment::book(&slot_id, user.get_id(), reason, &st.db).await?;
    Ok((
        StatusCode::CREATED,
        one_appointment(appointment, &st.db).await?,
    ))
}

/// List bookings, newest first. Students and parents see the ones they
/// requested; teacher+ see the ones aimed at their own slots — their request
/// inbox. Managers and admins read their own inbox too (they may still decide
/// any booking by id). Paged via `?limit=&offset=` (omit `limit` for every
/// booking); returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of bookings (all of them when unpaged)", body = Page<AppointmentResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_appointments(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<AppointmentResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let rows = if user.get_role().at_least(Role::Teacher) {
        Appointment::list_for_teacher(user.get_id(), &st.db).await?
    } else {
        Appointment::list_for_requester(user.get_id(), &st.db).await?
    };
    let total = rows.len() as i64;
    // Join slots and people onto the page alone — the lookup shrinks with it.
    let items = appointment_responses(paginate(&rows, limit, offset), &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Confirm a pending booking. Requires teacher+ and ownership of the slot (or
/// manager/admin). The double-booking guard runs here, against both sides:
/// approval is what commits anyone, so a time colliding with another approved
/// meeting of the teacher or of the requester is refused (`409`), as is a
/// window that has already started — that meeting could never be cancelled.
#[utoipa::path(
    patch,
    path = "/{id}/approve",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Appointment id")),
    responses(
        (status = 200, description = "Approved", body = AppointmentResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the slot's teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "No longer pending, the time has already started, or it collides with another approved appointment", body = ErrorResponse),
    ),
)]
async fn approve(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<AppointmentResponse>, AppError> {
    let id = AppointmentId::from_key(&id);
    for_decision(&id, &user, &st.db).await?;
    let appointment = Appointment::approve(&id, user.get_id(), &st.db).await?;
    one_appointment(appointment, &st.db).await
}

/// Turn a pending booking down. Requires teacher+ and ownership of the slot (or
/// manager/admin). The slot frees up immediately — occupancy counts live
/// bookings only, so someone else may take it.
#[utoipa::path(
    patch,
    path = "/{id}/reject",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Appointment id")),
    request_body(content = RejectRequest, description = "Optional rejection reason; the whole body may be omitted"),
    responses(
        (status = 200, description = "Rejected", body = AppointmentResponse),
        (status = 400, description = "Reason is over-long", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the slot's teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "No longer pending", body = ErrorResponse),
    ),
)]
async fn reject(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    body: Option<Json<RejectRequest>>,
) -> Result<Json<AppointmentResponse>, AppError> {
    let id = AppointmentId::from_key(&id);
    for_decision(&id, &user, &st.db).await?;
    // A blank or absent reason records none; a present one is validated (400 if
    // over-long) before it reaches the row.
    let reason = match body.and_then(|Json(req)| req.reason) {
        Some(reason) if !reason.trim().is_empty() => Some(AppointmentReason::try_new(&reason)?),
        _ => None,
    };
    let appointment = Appointment::reject(&id, user.get_id(), reason, &st.db).await?;
    one_appointment(appointment, &st.db).await
}

/// Call a meeting off. **The requester only** — the person who asked for it —
/// from either live state, so an approved meeting can still be dropped and the
/// slot freed. Anyone else is a `403`, the slot's teacher and a manager/admin
/// included (the guard compares ids, not roles): a teacher ends a booking by
/// **rejecting** it while it is `pending`, and an already-approved one by
/// counter-proposing another time (`PATCH /{id}/reschedule` — which sends it
/// back to `pending`) and then rejecting it. Refused (`409`) once the meeting's
/// window has started: a meeting that already began is history, not a plan.
#[utoipa::path(
    patch,
    path = "/{id}/cancel",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Appointment id")),
    request_body(content = CancelRequest, description = "Optional cancellation reason; the whole body may be omitted"),
    responses(
        (status = 200, description = "Cancelled", body = AppointmentResponse),
        (status = 400, description = "Reason is over-long", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the requester", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Already settled, or the appointment has already started", body = ErrorResponse),
    ),
)]
async fn cancel(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    body: Option<Json<CancelRequest>>,
) -> Result<Json<AppointmentResponse>, AppError> {
    let id = AppointmentId::from_key(&id);
    let appointment = Appointment::read(&id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // Only the requester (student/parent) may cancel. Teachers/managers end a
    // booking by rejecting (while pending) or rescheduling — never cancelling.
    if appointment.get_requester() != user.get_id() {
        return Err(AppError::Forbidden(
            "only the requester can cancel this appointment",
        ));
    }
    // A blank or absent reason records none; a present one is validated (400 if
    // over-long) before it reaches the row.
    let reason = match body.and_then(|Json(req)| req.reason) {
        Some(reason) if !reason.trim().is_empty() => Some(AppointmentReason::try_new(&reason)?),
        _ => None,
    };
    // The started-window guard lives in `Appointment::cancel`, under the lock
    // and on a fresh read — a pre-lock copy of it here would only be a staler
    // second opinion, and `decline_reschedule` would still bypass it.
    let appointment = Appointment::cancel(&id, user.get_id(), reason, &st.db).await?;
    one_appointment(appointment, &st.db).await
}

/// Counter-propose another time for a booking, on the same row (the reason and
/// the history stay in one place). Requires teacher+ and ownership of the slot
/// (or manager/admin).
///
/// **The proposal sends the booking back to `pending` and clears
/// `decided_by`** — an already-approved meeting is *not* committed at the new
/// time until the requester accepts it at
/// `PATCH /appointments/{id}/reschedule/accept`, since leaving it approved
/// would silently move a confirmed appointment. The slot stays held meanwhile.
///
/// Refused (`409`) when the proposed window has already opened — the 60-second
/// skew grace on the times is for clock drift, not for proposing into a meeting
/// that is already underway, which `cancel` would then refuse to undo.
#[utoipa::path(
    patch,
    path = "/{id}/reschedule",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Appointment id")),
    request_body = Reschedule,
    responses(
        (status = 200, description = "Proposed; the booking is pending again", body = AppointmentResponse),
        (status = 400, description = "Invalid time range or times in the past", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the slot's teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The appointment is already settled, or the proposed window has already started", body = ErrorResponse),
    ),
)]
async fn reschedule(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<Reschedule>,
) -> Result<Json<AppointmentResponse>, AppError> {
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = Timestamp::from_millis(req.ends_at);
    check_not_past("starts_at", Some(starts_at))?;
    check_not_past("ends_at", Some(ends_at))?;
    check_time_range(Some(starts_at), Some(ends_at))?;

    let id = AppointmentId::from_key(&id);
    for_decision(&id, &user, &st.db).await?;
    let appointment = Appointment::propose(&id, starts_at, ends_at, user.get_id(), &st.db).await?;
    one_appointment(appointment, &st.db).await
}

/// Accept the teacher's counter-proposal. The requester's call alone — it is
/// their commitment. Accepting *is* approval at the proposed time, so the
/// double-booking guard runs again for both sides (`409` if the moved time now
/// collides with something else, or has already started — agreeing to a window
/// that began would mint a meeting nobody can cancel).
#[utoipa::path(
    patch,
    path = "/{id}/reschedule/accept",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Appointment id")),
    responses(
        (status = 200, description = "Approved at the proposed time", body = AppointmentResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the requester", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "No time has been proposed, the booking is no longer pending, the proposed time has already started, or it collides with another approved appointment", body = ErrorResponse),
    ),
)]
async fn accept_reschedule(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<AppointmentResponse>, AppError> {
    let id = AppointmentId::from_key(&id);
    ensure_requester(&id, &user, &st.db).await?;
    let appointment = Appointment::accept_proposal(&id, user.get_id(), &st.db).await?;
    one_appointment(appointment, &st.db).await
}

/// Refuse the teacher's counter-proposal. The requester's call alone, and it
/// **cancels the booking**: the proposal replaced the time that was asked for,
/// so there is nothing left to fall back to — book another slot instead. The
/// slot frees up, and the original request stays readable as `cancelled` with
/// the refused proposal still on it. Declining is a cancel, so it answers to
/// the same deadline: `409` once the meeting's effective window has started.
#[utoipa::path(
    patch,
    path = "/{id}/reschedule/decline",
    tag = "appointments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Appointment id")),
    responses(
        (status = 200, description = "Cancelled", body = AppointmentResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the requester", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "No time has been proposed, the booking is already settled, or the meeting's window has already started", body = ErrorResponse),
    ),
)]
async fn decline_reschedule(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<AppointmentResponse>, AppError> {
    let id = AppointmentId::from_key(&id);
    let appointment = ensure_requester(&id, &user, &st.db).await?;
    if appointment.get_proposed_starts_at().is_none() {
        return Err(AppError::Conflict("no time has been proposed"));
    }
    let appointment = Appointment::cancel(&id, user.get_id(), None, &st.db).await?;
    one_appointment(appointment, &st.db).await
}

/// The booking, insisting the caller is the person who asked for it — the only
/// one who can answer a counter-proposal.
async fn ensure_requester(
    id: &AppointmentId,
    user: &User,
    db: &Database,
) -> Result<Appointment, AppError> {
    let appointment = Appointment::read(id, db).await?.ok_or(AppError::NotFound)?;
    if appointment.get_requester() != user.get_id() {
        return Err(AppError::Forbidden(
            "only the requester can answer a counter-proposal",
        ));
    }
    Ok(appointment)
}
