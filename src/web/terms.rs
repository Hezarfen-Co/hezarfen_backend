use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::term::{Term, TermId, TermName};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;

use super::{CurrentUser, Page, PageParams, RequireManager, check_time_range};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_term, list_terms))
        .routes(routes!(get_term, update_term, delete_term))
        .routes(routes!(archive_term))
        .routes(routes!(unarchive_term))
}

#[derive(Deserialize, ToSchema)]
struct CreateTerm {
    #[schema(example = "1. Dönem", max_length = 100)]
    name: String,
    /// The academic year this term is a slice of (`GET /academic-years`).
    /// Required — a term outside a year has no report card to be counted into.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    year: String,
    /// Term start, UTC unix-milliseconds. May lie in the past — a school
    /// adopting the app mid-year backfills its calendar legitimately.
    #[schema(example = 1_780_000_000_000_i64)]
    starts_at: i64,
    /// Term end, UTC unix-milliseconds; must not precede `starts_at`.
    #[schema(example = 1_790_000_000_000_i64)]
    ends_at: i64,
}

#[derive(Deserialize, ToSchema)]
struct UpdateTerm {
    #[schema(max_length = 100)]
    name: Option<String>,
    starts_at: Option<i64>,
    ends_at: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct TermResponse {
    id: String,
    #[schema(example = "1. Dönem")]
    name: String,
    /// The academic year this term belongs to (`GET /academic-years/{id}`).
    year: String,
    /// Term start, UTC unix-milliseconds.
    starts_at: i64,
    /// Term end, UTC unix-milliseconds.
    ends_at: i64,
    /// Archived at, UTC unix-millis; null while the term is open.
    archived_at: Option<i64>,
}

impl TermResponse {
    fn new(term: &Term) -> Self {
        Self {
            id: term.get_id().key().to_string(),
            name: term.get_name().as_str().to_string(),
            year: term.get_year().key().to_string(),
            starts_at: term.get_starts_at().as_millis(),
            ends_at: term.get_ends_at().as_millis(),
            archived_at: term.get_archived_at().map(|at| at.as_millis()),
        }
    }
}

/// Create a term inside an academic year. Requires manager+. Past dates are
/// allowed — terms are calendar structure, not schedules; an *archived* year
/// refuses the new term (`409`), because past years take no new structure.
#[utoipa::path(
    post,
    path = "/",
    tag = "terms",
    security(("session_cookie" = [])),
    request_body = CreateTerm,
    responses(
        (status = 201, description = "Term created", body = TermResponse),
        (status = 400, description = "Invalid name or range, or an unknown year", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "The named academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(req): Json<CreateTerm>,
) -> Result<(StatusCode, Json<TermResponse>), AppError> {
    let name = TermName::try_new(&req.name)?;
    // The single spot a request-supplied year passes through: an unknown id is
    // a 400 naming the field, an archived one the year's own 409.
    let Some(year) = service::academic_year::resolve(&st.db, Some(&req.year)).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "year",
            reason: "academic year is required",
        }));
    };
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = Timestamp::from_millis(req.ends_at);
    check_time_range(Some(starts_at), Some(ends_at))?;
    let term = service::term::create(&st.db, name, year, starts_at, ends_at).await?;
    Ok((StatusCode::CREATED, Json(TermResponse::new(&term))))
}

/// List every term, newest first. Any authenticated user — students need the
/// calendar to make sense of their courses. Paged via `?limit=&offset=` (omit
/// `limit` for the full list); returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "terms",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of terms (the full list when unpaged)", body = Page<TermResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_terms(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<TermResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (terms, total) = service::term::list_all(&st.db, limit, offset).await?;
    let items = terms.iter().map(TermResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single term by id.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "terms",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Term id")),
    responses(
        (status = 200, description = "The term", body = TermResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_term(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<TermResponse>, AppError> {
    let term = service::term::read(&st.db, &TermId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(TermResponse::new(&term)))
}

/// Update a term. Requires manager+. Omitted fields keep their value; the
/// merged range must stay ordered.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "terms",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Term id")),
    request_body = UpdateTerm,
    responses(
        (status = 200, description = "Updated term", body = TermResponse),
        (status = 400, description = "Invalid name or range", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The term is archived, or its academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateTerm>,
) -> Result<Json<TermResponse>, AppError> {
    let name = req.name.as_deref().map(TermName::try_new).transpose()?;
    let starts_at = req.starts_at.map(Timestamp::from_millis);
    let ends_at = req.ends_at.map(Timestamp::from_millis);

    let term = service::term::read(&st.db, &TermId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    service::term::require_writable(&term)?;
    // The year above the term is the other half of the read-only rule, and
    // the half a closed term no longer covers: a term still open inside an
    // archived year takes no edit either.
    service::academic_year::require_open(&st.db, term.get_year()).await?;
    // Pre-flight only: the range check is re-made inside the UPDATE's own
    // `WHERE` (the term update in the db layer), so a concurrent move of the
    // end this PATCH omits cannot slip an inverted range past this snapshot.
    check_time_range(
        Some(starts_at.unwrap_or_else(|| term.get_starts_at())),
        Some(ends_at.unwrap_or_else(|| term.get_ends_at())),
    )?;

    let updated = service::term::update(&st.db, term, name, starts_at, ends_at).await?;
    Ok(Json(TermResponse::new(&updated)))
}

/// Delete a term. Requires manager+. Refused with a 409 while anything still
/// hangs off it — an exam filed in it, or a report card frozen for it — so a
/// term is never dropped out from under marks that name it; move or delete
/// those first. The term's own academic year is untouched (that is `DELETE
/// /academic-years/{id}`, which refuses while a term still links it).
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "terms",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Term id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Exams or frozen karnes still belong to this term, the term is archived, or its academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let term = service::term::read(&st.db, &TermId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    service::term::require_writable(&term)?;
    service::academic_year::require_open(&st.db, term.get_year()).await?;
    service::term::delete(&st.db, term).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Archive a term. Requires manager+. Archiving **freezes the report cards**:
/// every student with a roster row under the term's year gets a snapshot of
/// their report, and from then on `GET /marks/karne` serves that record instead
/// of recomputing — a mark corrected after the fact no longer rewrites what a
/// family holds. An archived term takes no edits and no delete; exams may
/// still be created in it while its *year* is open (the archive is a record,
/// not a wall). Idempotent — archiving an already-archived term answers `200`
/// with the stamp it already had and never re-freezes.
#[utoipa::path(
    post,
    path = "/{id}/archive",
    tag = "terms",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Term id")),
    responses(
        (status = 200, description = "The archived term", body = TermResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn archive_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<Json<TermResponse>, AppError> {
    let term = service::term::read(&st.db, &TermId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let archived = service::term::archive(&st.db, term).await?;
    Ok(Json(TermResponse::new(&archived)))
}

/// Re-open an archived term. Requires manager+. Idempotent the same way as
/// archiving: an already-open term answers `200`.
#[utoipa::path(
    post,
    path = "/{id}/unarchive",
    tag = "terms",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Term id")),
    responses(
        (status = 200, description = "The re-opened term", body = TermResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn unarchive_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<Json<TermResponse>, AppError> {
    let term = service::term::read(&st.db, &TermId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let reopened = service::term::unarchive(&st.db, term).await?;
    Ok(Json(TermResponse::new(&reopened)))
}
