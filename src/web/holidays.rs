//! The holiday calendar's HTTP surface: the school-wide non-teaching ranges
//! the weekly-plan materializer bounds its generation with. Plain CRUD — the
//! rows carry no refcounts and nothing else writes them — so every handler is
//! the same shape as `terms`': manager+ on the writes, any session on the
//! reads, an ungated nest because the calendar is school structure rather than
//! a per-role feature.
//!
//! `starts_at`/`ends_at` are **instants** (UTC unix milliseconds) exactly like
//! every other dated resource here; the list's `?from=&to=` bounds are
//! inclusive and use *overlap* semantics — a holiday that merely reaches into
//! the window is listed.

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::db::holiday::HolidayPatch;
use crate::domain::holiday::{Holiday, HolidayId, HolidayKind, HolidayName};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::state::AppState;

use super::{CurrentUser, Page, PageParams, PersonRef, RequireManager, check_time_range, person_map};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_holiday, list_holidays))
        .routes(routes!(get_holiday, update_holiday, delete_holiday))
}

#[derive(Deserialize, ToSchema)]
struct CreateHoliday {
    /// The holiday's display name ("29 Ekim Cumhuriyet Bayramı").
    #[schema(example = "29 Ekim Cumhuriyet Bayramı", max_length = 100)]
    name: String,
    /// First blocked instant, UTC unix-milliseconds.
    #[schema(example = 1_793_318_400_000_i64)]
    starts_at: i64,
    /// Last blocked instant, UTC unix-milliseconds; must not precede
    /// `starts_at`. A single-day holiday sets both ends to that day's bounds.
    #[schema(example = 1_793_404_800_000_i64)]
    ends_at: i64,
    /// Why the school is closed: one of `HOLIDAY_KINDS` (`GET /limits`
    /// publishes them).
    #[schema(example = "resmi")]
    kind: String,
}

#[derive(Deserialize, ToSchema)]
struct UpdateHoliday {
    /// New name, held to the same published bound as `POST` — and still
    /// re-checked by the `HolidayName` newtype at runtime.
    #[schema(max_length = 100)]
    name: Option<String>,
    #[schema(example = 1_793_318_400_000_i64)]
    starts_at: Option<i64>,
    #[schema(example = 1_793_404_800_000_i64)]
    ends_at: Option<i64>,
    #[schema(example = "idari")]
    kind: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct HolidayResponse {
    id: String,
    #[schema(example = "29 Ekim Cumhuriyet Bayramı")]
    name: String,
    /// First blocked instant, UTC unix-milliseconds.
    starts_at: i64,
    /// Last blocked instant, UTC unix-milliseconds.
    ends_at: i64,
    #[schema(example = "resmi")]
    kind: String,
    /// The manager who declared the holiday (`GET /users/{id}`).
    creator: PersonRef,
    /// Declared at, UTC unix-milliseconds.
    created_at: i64,
}

impl HolidayResponse {
    fn new(holiday: &Holiday, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: holiday.get_id().key().to_string(),
            name: holiday.get_name().as_str().to_string(),
            starts_at: holiday.get_starts_at().as_millis(),
            ends_at: holiday.get_ends_at().as_millis(),
            kind: holiday.get_kind().as_str().to_string(),
            creator: PersonRef::resolve(people, holiday.get_creator()),
            created_at: holiday.get_created_at().as_millis(),
        }
    }
}

/// Inclusive instant bounds on `GET /holidays`. Either may be omitted; a
/// holiday is listed when it *reaches into* the window, not only when it lies
/// entirely inside it.
#[derive(Debug, Deserialize, IntoParams)]
struct HolidayRange {
    /// Keep holidays reaching at or after this instant, unix-milliseconds.
    #[param(example = 1_793_318_400_000_i64)]
    from: Option<i64>,
    /// Keep holidays reaching at or before this instant, unix-milliseconds.
    #[param(example = 1_793_404_800_000_i64)]
    to: Option<i64>,
}

/// Every holiday the request named — the join half of the responses below.
async fn people_of(
    holidays: &[Holiday],
    db: &crate::database::Database,
) -> Result<std::collections::HashMap<String, PersonRef>, AppError> {
    person_map(holidays.iter().map(|holiday| *holiday.get_creator()), db).await
}

/// Declare a school-wide holiday. Requires manager+. Past dates are allowed —
/// the calendar is a record, and a school adopting the app mid-year backfills
/// it legitimately.
#[utoipa::path(
    post,
    path = "/",
    tag = "holidays",
    security(("session_cookie" = [])),
    request_body = CreateHoliday,
    responses(
        (status = 201, description = "Holiday declared", body = HolidayResponse),
        (status = 400, description = "Invalid name, kind, or range", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_holiday(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateHoliday>,
) -> Result<(StatusCode, Json<HolidayResponse>), AppError> {
    let name = HolidayName::try_new(&req.name)?;
    let kind = HolidayKind::try_new(&req.kind)?;
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = Timestamp::from_millis(req.ends_at);
    check_time_range(Some(starts_at), Some(ends_at))?;
    let holiday = service::holiday::create(
        &st.db,
        &name,
        starts_at,
        ends_at,
        &kind,
        user.get_id(),
    )
    .await?;
    // The creator is the caller — already loaded, no extra lookup.
    let people = PersonRef::map_of(&[&user]);
    Ok((
        StatusCode::CREATED,
        Json(HolidayResponse::new(&holiday, &people)),
    ))
}

/// List the calendar, newest start first. Any authenticated user — students
/// need the calendar to make sense of their courses. Paged via
/// `?limit=&offset=` (omit `limit` for the full list), optionally windowed by
/// `?from=&to=`; returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "holidays",
    security(("session_cookie" = [])),
    params(HolidayRange, PageParams),
    responses(
        (status = 200, description = "A page of holidays (the full list when unpaged)", body = Page<HolidayResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_holidays(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Query(range): Query<HolidayRange>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<HolidayResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (holidays, total) = service::holiday::list(
        &st.db,
        range.from.map(Timestamp::from_millis),
        range.to.map(Timestamp::from_millis),
        limit,
        offset,
    )
    .await?;
    let people = people_of(&holidays, &st.db).await?;
    let items = holidays
        .iter()
        .map(|holiday| HolidayResponse::new(holiday, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single holiday by id.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "holidays",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Holiday id")),
    responses(
        (status = 200, description = "The holiday", body = HolidayResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_holiday(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<HolidayResponse>, AppError> {
    let holiday = service::holiday::read(&st.db, &HolidayId::from_key(&id)).await?;
    let people = people_of(std::slice::from_ref(&holiday), &st.db).await?;
    Ok(Json(HolidayResponse::new(&holiday, &people)))
}

/// Update a holiday. Requires manager+. Omitted fields keep their value; the
/// merged range must stay ordered. A holiday is a *record* of the calendar,
/// so a past range is editable like a future one.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "holidays",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Holiday id")),
    request_body = UpdateHoliday,
    responses(
        (status = 200, description = "Updated holiday", body = HolidayResponse),
        (status = 400, description = "Invalid name, kind, or range", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_holiday(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateHoliday>,
) -> Result<Json<HolidayResponse>, AppError> {
    let name = req.name.as_deref().map(HolidayName::try_new).transpose()?;
    let kind = req.kind.as_deref().map(HolidayKind::try_new).transpose()?;
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    let ends_at = req.ends_at.map(Timestamp::from_millis);
    // Pre-flight only: the merged range is re-checked inside the UPDATE's own
    // `WHERE` (the holiday update in the db layer), so a concurrent move of
    // the end this PATCH omits cannot slip an inverted range past this
    // snapshot.
    check_time_range(starts_at, ends_at)?;

    let patch = HolidayPatch {
        name,
        starts_at,
        ends_at,
        kind,
    };
    let holiday = service::holiday::update(&st.db, &HolidayId::from_key(&id), patch).await?;
    let people = people_of(std::slice::from_ref(&holiday), &st.db).await?;
    Ok(Json(HolidayResponse::new(&holiday, &people)))
}

/// Delete a holiday. Requires manager+. Nothing references a holiday — the
/// materializer only *reads* the calendar — so the delete is unconditional and
/// the lessons already generated on those days stay where they are.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "holidays",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Holiday id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_holiday(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    service::holiday::delete(&st.db, &HolidayId::from_key(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
