use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::term::{TERM_LOCK, Term, TermId, TermName};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{CurrentUser, Page, PageParams, RequireManager, check_time_range, paginate};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_term, list_terms))
        .routes(routes!(get_term, update_term, delete_term))
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
}

impl TermResponse {
    fn new(term: &Term) -> Self {
        Self {
            id: term.get_id().key().to_string(),
            name: term.get_name().as_str().to_string(),
            starts_at: term.get_starts_at().as_millis(),
            ends_at: term.get_ends_at().as_millis(),
        }
    }
}

/// Turn an optional request-supplied term id into a validated reference —
/// `None` stays `None`, an unknown id is a `400` naming the field. Shared by
/// the course create/update handlers.
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
    let terms = Term::list_all(&st.db).await?;
    let total = terms.len() as i64;
    let items = paginate(&terms, limit, offset)
        .iter()
        .map(TermResponse::new)
        .collect();
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

    // Only the range check needs the stored row, and only it can race: it
    // validates an arriving end against the other end as stored, so the read,
    // the check and the write are held together under [`TERM_LOCK`].
    let _guard = match (starts_at, ends_at) {
        (None, None) => None,
        _ => Some(TERM_LOCK.lock().await),
    };
    let term = Term::read(&TermId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
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
        (status = 409, description = "Courses are still linked to this term", body = ErrorResponse),
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
    // [`TERM_LOCK`] holds the link check and the delete together, so a course
    // write that just resolved this term can't land its link on a dead row.
    let _guard = TERM_LOCK.lock().await;
    if Term::any_course(term.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "courses are still linked to this term — unlink them first",
        ));
    }
    term.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
