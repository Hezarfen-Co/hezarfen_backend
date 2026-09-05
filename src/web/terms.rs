use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::term::{self, Term, TermId, TermName};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ErrorResponse, ValidationError};
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
    #[schema(example = "2026 Fall", max_length = 100)]
    name: String,
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
    #[schema(example = "2026 Fall")]
    name: String,
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
            starts_at: term.get_starts_at().as_millis(),
            ends_at: term.get_ends_at().as_millis(),
            archived_at: term.get_archived_at().map(|at| at.as_millis()),
        }
    }
}

/// Turn an optional request-supplied term id into a validated reference —
/// `None` stays `None`, an unknown id is a `400` naming the field, and an
/// *archived* one is a `409 term_archived`. That last refusal is here rather
/// than in each handler because this is the single spot every new link to a
/// term passes through — course create/update and class create/update alike:
/// past years take no new structure.
pub(crate) async fn resolve_term(
    id: Option<&str>,
    db: &Database,
) -> Result<Option<TermId>, AppError> {
    let Some(id) = id else {
        return Ok(None);
    };
    let term = Term::read(&TermId::from_key(id), db)
        .await?
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "term_id",
            reason: "term does not exist",
        }))?;
    if term.is_archived() {
        return Err(term::archived_error());
    }
    Ok(Some(term.get_id().clone()))
}

/// Create an academic term. Requires manager+. Past dates are allowed —
/// terms are calendar structure, not schedules.
#[utoipa::path(
    post,
    path = "/",
    tag = "terms",
    security(("session_cookie" = [])),
    request_body = CreateTerm,
    responses(
        (status = 201, description = "Term created", body = TermResponse),
        (status = 400, description = "Invalid name or range", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(req): Json<CreateTerm>,
) -> Result<(StatusCode, Json<TermResponse>), AppError> {
    let name = TermName::try_new(&req.name)?;
    let starts_at = Timestamp::from_millis(req.starts_at);
    let ends_at = Timestamp::from_millis(req.ends_at);
    check_time_range(Some(starts_at), Some(ends_at))?;
    let term = Term::create(name, starts_at, ends_at, &st.db).await?;
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
    let (terms, total) = Term::list_all(limit, offset, &st.db).await?;
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
    let term = Term::read(&TermId::from_key(&id), &st.db)
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
        (status = 409, description = "The term is archived — past years are read-only", body = ErrorResponse),
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

    let term = Term::read(&TermId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if term.is_archived() {
        return Err(term::archived_error());
    }
    // Pre-flight only: the range check is re-made inside the UPDATE's `WHERE`
    // (`Term::update`), so a concurrent move of the end this PATCH omits cannot
    // slip an inverted range past this snapshot.
    check_time_range(
        Some(starts_at.unwrap_or_else(|| term.get_starts_at())),
        Some(ends_at.unwrap_or_else(|| term.get_ends_at())),
    )?;

    let updated = term.update(name, starts_at, ends_at, &st.db).await?;
    Ok(Json(TermResponse::new(&updated)))
}

/// Delete a term. Requires manager+. Refused with a 409 while any course still
/// links to it — unlink those courses (`PATCH /courses/{id}` with
/// `"term_id": null`) or delete them first, so a term is never dropped out from
/// under the calendar its courses hang on.
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
        (status = 409, description = "Courses are still linked to this term, or the term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_term(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let term = Term::read(&TermId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if term.is_archived() {
        return Err(term::archived_error());
    }
    if !term.delete(&st.db).await? {
        return Err(AppError::Conflict(
            "courses are still linked to this term — unlink them first",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Archive a term. Requires manager+. An archived term is frozen: it takes no
/// edits, no delete, and no new course or class link. Idempotent — archiving an
/// already-archived term answers `200` with the stamp it already had.
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
    let term = Term::read(&TermId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let archived = term.archive(&st.db).await?;
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
    let term = Term::read(&TermId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let reopened = term.unarchive(&st.db).await?;
    Ok(Json(TermResponse::new(&reopened)))
}
