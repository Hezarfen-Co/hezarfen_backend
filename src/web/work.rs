use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::domain::work_entry::{WorkEntry, WorkEntryId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{RequireManager, RequireTeacher};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(check_in))
        .routes(routes!(check_out))
        .routes(routes!(my_work))
        .routes(routes!(user_work))
        .routes(routes!(update_entry, delete_entry))
}

#[derive(Deserialize, ToSchema)]
struct UpdateWorkEntry {
    /// Corrected check-in, UTC unix-milliseconds. Omit to keep.
    check_in: Option<i64>,
    /// Corrected check-out, UTC unix-milliseconds. Omit to keep.
    check_out: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct WorkEntryResponse {
    id: String,
    /// The staff member this stint belongs to.
    user: String,
    /// Stint start, UTC unix-milliseconds (server-stamped).
    check_in: i64,
    /// Stint end, UTC unix-milliseconds; `null` while still checked in.
    check_out: Option<i64>,
    /// `check_out - check_in`; `null` while still checked in.
    duration_ms: Option<i64>,
}

impl WorkEntryResponse {
    fn new(entry: &WorkEntry) -> Self {
        let check_in = entry.get_check_in().as_millis();
        let check_out = entry.get_check_out().map(|t| t.as_millis());
        Self {
            id: entry.get_id().key().to_string(),
            user: entry.get_user().key().to_string(),
            check_in,
            check_out,
            duration_ms: check_out.map(|out| out - check_in),
        }
    }
}

/// Check in for work. Requires teacher+ (staff). The instant is stamped by the
/// server clock — clients never supply it. `409` while already checked in.
#[utoipa::path(
    post,
    path = "/check-in",
    tag = "work",
    security(("session_cookie" = [])),
    responses(
        (status = 201, description = "Checked in — the open stint", body = WorkEntryResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 409, description = "Already checked in", body = ErrorResponse),
    ),
)]
async fn check_in(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
) -> Result<(StatusCode, Json<WorkEntryResponse>), AppError> {
    let entry = WorkEntry::check_in(user.get_id(), &st.db).await?;
    Ok((StatusCode::CREATED, Json(WorkEntryResponse::new(&entry))))
}

/// Check out of work, closing the open stint. Requires teacher+ (staff). The
/// instant is stamped by the server clock. `409` when not checked in.
#[utoipa::path(
    post,
    path = "/check-out",
    tag = "work",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "Checked out — the closed stint", body = WorkEntryResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 409, description = "Not checked in", body = ErrorResponse),
    ),
)]
async fn check_out(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
) -> Result<Json<WorkEntryResponse>, AppError> {
    let entry = WorkEntry::check_out(user.get_id(), &st.db).await?;
    Ok(Json(WorkEntryResponse::new(&entry)))
}

/// The caller's own work log, newest first — the open stint (if any) included
/// (`check_out: null`). Requires teacher+ (staff).
#[utoipa::path(
    get,
    path = "/me",
    tag = "work",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's work log", body = [WorkEntryResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn my_work(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
) -> Result<Json<Vec<WorkEntryResponse>>, AppError> {
    let entries = WorkEntry::list_for_user(user.get_id(), &st.db).await?;
    Ok(Json(entries.iter().map(WorkEntryResponse::new).collect()))
}

/// A staff member's work log. Requires manager+.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "work",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's work log", body = [WorkEntryResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn user_work(
    State(st): State<AppState>,
    _manager: RequireManager,
    Path(user): Path<String>,
) -> Result<Json<Vec<WorkEntryResponse>>, AppError> {
    let target = UserId::from_key(&user);
    // User must exist — a missing user is a 404, not an empty log.
    User::read(&target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let entries = WorkEntry::list_for_user(&target, &st.db).await?;
    Ok(Json(entries.iter().map(WorkEntryResponse::new).collect()))
}

/// Correct a closed stint's instants. Requires manager+. An open stint can't
/// be corrected (`409`) — it has no end yet; check out first (or delete it).
#[utoipa::path(
    patch,
    path = "/entries/{id}",
    tag = "work",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Work entry id")),
    request_body = UpdateWorkEntry,
    responses(
        (status = 200, description = "Corrected entry", body = WorkEntryResponse),
        (status = 400, description = "check_out precedes check_in", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Entry is still open", body = ErrorResponse),
    ),
)]
async fn update_entry(
    State(st): State<AppState>,
    _manager: RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateWorkEntry>,
) -> Result<Json<WorkEntryResponse>, AppError> {
    let entry = WorkEntry::read(&WorkEntryId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let Some(current_out) = entry.get_check_out() else {
        return Err(AppError::Conflict(
            "entry is still open — check out first, or delete it",
        ));
    };

    let check_in = req
        .check_in
        .map(Timestamp::from_millis)
        .unwrap_or_else(|| entry.get_check_in());
    let check_out = req
        .check_out
        .map(Timestamp::from_millis)
        .unwrap_or(current_out);
    if check_out < check_in {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "check_out",
            reason: "must be at or after check_in",
        }));
    }

    let updated = entry.update(check_in, check_out, &st.db).await?;
    Ok(Json(WorkEntryResponse::new(&updated)))
}

/// Delete a work entry (open or closed). Requires manager+.
#[utoipa::path(
    delete,
    path = "/entries/{id}",
    tag = "work",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Work entry id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_entry(
    State(st): State<AppState>,
    _manager: RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = WorkEntry::remove(&WorkEntryId::from_key(&id), &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
