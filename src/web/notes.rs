use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::note::{Note, NoteContent, NoteId, NoteTitle};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::CurrentUser;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(get_one, update, delete_one))
}

#[derive(Deserialize, ToSchema)]
struct CreateNote {
    #[schema(example = "Groceries")]
    title: String,
    #[schema(example = "milk, eggs")]
    content: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateNote {
    title: Option<String>,
    content: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct NoteResponse {
    id: String,
    title: String,
    content: String,
}

impl NoteResponse {
    fn new(note: &Note) -> Self {
        Self {
            id: note.get_id().key().to_string(),
            title: note.get_title().as_str().to_string(),
            content: note.get_content().as_str().to_string(),
        }
    }
}

/// Create a note owned by the current user.
#[utoipa::path(
    post,
    path = "/",
    tag = "notes",
    security(("session_cookie" = [])),
    request_body = CreateNote,
    responses(
        (status = 201, description = "Note created", body = NoteResponse),
        (status = 400, description = "Invalid title or content", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn create(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<CreateNote>,
) -> Result<(StatusCode, Json<NoteResponse>), AppError> {
    let title = NoteTitle::try_new(&req.title)?;
    let content = NoteContent::try_new(&req.content.unwrap_or_default())?;
    let note = Note::create(user.get_id(), title, content, &st.db).await?;
    Ok((StatusCode::CREATED, Json(NoteResponse::new(&note))))
}

/// List all notes owned by the current user.
#[utoipa::path(
    get,
    path = "/",
    tag = "notes",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The user's notes", body = [NoteResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<NoteResponse>>, AppError> {
    let notes = Note::list_for(user.get_id(), &st.db).await?;
    Ok(Json(notes.iter().map(NoteResponse::new).collect()))
}

/// Fetch a single note by id (must be owned by the current user).
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Note id")),
    responses(
        (status = 200, description = "The note", body = NoteResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_one(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<NoteResponse>, AppError> {
    let note = Note::read_owned(&NoteId::from_key(&id), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(NoteResponse::new(&note)))
}

/// Update a note's title and/or content. Omitted fields keep their value.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Note id")),
    request_body = UpdateNote,
    responses(
        (status = 200, description = "Updated note", body = NoteResponse),
        (status = 400, description = "Invalid title or content", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn update(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateNote>,
) -> Result<Json<NoteResponse>, AppError> {
    let note = Note::read_owned(&NoteId::from_key(&id), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let title = match req.title {
        Some(ref title) => NoteTitle::try_new(title)?,
        None => note.get_title().clone(),
    };
    let content = match req.content {
        Some(ref content) => NoteContent::try_new(content)?,
        None => note.get_content().clone(),
    };

    let updated = note.update(title, content, &st.db).await?;
    Ok(Json(NoteResponse::new(&updated)))
}

/// Delete a note owned by the current user.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Note id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_one(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let note = Note::read_owned(&NoteId::from_key(&id), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    note.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
