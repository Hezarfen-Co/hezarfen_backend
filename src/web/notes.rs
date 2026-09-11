use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::domain::note::{Note, NoteContent, NoteId, NoteTitle};
use crate::domain::note_file::{FileContentType, FileName, NoteFile, NoteFileId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::state::AppState;

use super::{CurrentUser, Page, PageParams, UploadFileForm, blob_path, read_upload, remove_blob};

pub fn routes() -> OpenApiRouter<AppState> {
    // The file routes get their own HTTP body cap: the server-wide hard
    // ceiling plus multipart framing headroom (axum's 2 MB default would
    // reject legal uploads). The school's actual — usually smaller — limit
    // from settings is enforced while the stream is read.
    let files = OpenApiRouter::new()
        .routes(routes!(upload_file, list_files))
        .routes(routes!(download_file, delete_file))
        .layer(DefaultBodyLimit::max(
            MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
        ));
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(get_one, update, delete_one))
        .merge(files)
}

#[derive(Deserialize, ToSchema)]
struct CreateNote {
    #[schema(example = "Groceries", max_length = 200)]
    title: String,
    #[schema(example = "milk, eggs", max_length = 10000)]
    content: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateNote {
    #[schema(max_length = 200)]
    title: Option<String>,
    #[schema(max_length = 10000)]
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
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<CreateNote>,
) -> Result<(StatusCode, Json<NoteResponse>), AppError> {
    let title = NoteTitle::try_new(&req.title)?;
    let content = NoteContent::try_new(&req.content.unwrap_or_default())?;
    let note = service::note::create(&st.db, user.get_id(), title, content).await?;
    Ok((StatusCode::CREATED, Json(NoteResponse::new(&note))))
}

/// List the notes owned by the current user, newest first. Paged via
/// `?limit=&offset=` (omit `limit` for all of them); returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "notes",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the user's notes (all of them when unpaged)", body = Page<NoteResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<NoteResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (notes, total) = service::note::list_for(&st.db, user.get_id(), limit, offset).await?;
    let items = notes.iter().map(NoteResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
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
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
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
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateNote>,
) -> Result<Json<NoteResponse>, AppError> {
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;

    // Only what the request carried: an omitted field stays `None` and is
    // never written, so a concurrent PATCH of the other field survives. (A
    // JSON `null` deserializes to `None` too — neither column is nullable, so
    // "omitted" and "null" both mean "keep it".)
    let title = req.title.as_deref().map(NoteTitle::try_new).transpose()?;
    let content = req
        .content
        .as_deref()
        .map(NoteContent::try_new)
        .transpose()?;

    let updated = service::note::update(&st.db, note, title, content).await?;
    Ok(Json(NoteResponse::new(&updated)))
}

/// Delete a note owned by the current user, along with its files.
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
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    // Rows go first (the note delete cascades them), blobs after: a crash in
    // between strands at worst an unreachable blob, never a row whose blob is
    // already gone. The files to unlink come from the delete itself, not a
    // pre-read list — an upload that landed in between is in the cascade too.
    let (_, files) = service::note::delete(&st.db, note).await?;
    for file in &files {
        remove_blob(&st.files_path, file.get_id().key()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A note file's stored metadata; the bytes themselves come from the
/// download endpoint.
#[derive(Serialize, ToSchema)]
struct NoteFileResponse {
    id: String,
    /// The uploader's original filename.
    #[schema(example = "homework.pdf")]
    name: String,
    /// MIME type as declared on upload.
    #[schema(example = "application/pdf")]
    content_type: String,
    /// File size in bytes.
    #[schema(example = 24_576)]
    size: i64,
}

impl NoteFileResponse {
    fn new(file: &NoteFile) -> Self {
        Self {
            id: file.get_id().key().to_string(),
            name: file.get_name().as_str().to_string(),
            content_type: file.get_content_type().as_str().to_string(),
            size: file.get_size(),
        }
    }
}

/// Attach a file to a note owned by the current user. `multipart/form-data`
/// with the file under a `file` field; its `filename` is required. At most
/// 10 files per note; each file at most the school's `max_file_bytes`
/// (settings, default 5 MiB).
#[utoipa::path(
    post,
    path = "/{id}/files",
    tag = "notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Note id")),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "File stored", body = NoteFileResponse),
        (status = 400, description = "Missing file field, invalid filename or content type, or empty file", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Note not found", body = ErrorResponse),
        (status = 409, description = "The note already holds the maximum number of files", body = ErrorResponse),
        (status = 413, description = "File exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<NoteFileResponse>), AppError> {
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    // The 10-file cap is enforced inside `service::note_file::insert` (count
    // and create in one conditional write) — checking it here too would just
    // race.
    let limit = service::settings::load(&st.db).await?.get_max_file_bytes();

    let upload = read_upload(&mut multipart, limit).await?;
    let name = FileName::try_new(&upload.name.unwrap_or_default())?;
    let content_type = FileContentType::try_new(&upload.content_type.unwrap_or_default())?;

    // Blob first, row second — a stored row always points at a real blob. If
    // the row insert fails, take the fresh blob back out.
    let file = NoteFile::new(note.get_id(), name, content_type, upload.data.len() as i64);
    let path = blob_path(&st.files_path, file.get_id().key());
    crate::web::ensure_files_dir(&st.files_path).await?;
    tokio::fs::write(&path, &upload.data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the file blob: {err}")))?;
    match service::note_file::insert(&st.db, file).await {
        Ok(created) => Ok((StatusCode::CREATED, Json(NoteFileResponse::new(&created)))),
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err)
        }
    }
}

/// List a note's files (metadata only), newest first. Paged via
/// `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/files",
    tag = "notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Note id"), PageParams),
    responses(
        (status = 200, description = "A page of the note's files", body = Page<NoteFileResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Note not found", body = ErrorResponse),
    ),
)]
async fn list_files(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<NoteFileResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let (files, total) = service::note_file::list_for(&st.db, note.get_id(), limit, offset).await?;
    let items = files.iter().map(NoteFileResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Download a note file's bytes. `Content-Type` is the one declared on
/// upload; `Content-Disposition` carries the original filename.
#[utoipa::path(
    get,
    path = "/{id}/files/{file_id}",
    tag = "notes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Note id"),
        ("file_id" = String, Path, description = "File id"),
    ),
    responses(
        (status = 200, description = "The file bytes", content_type = "application/octet-stream"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Note or file not found", body = ErrorResponse),
    ),
)]
async fn download_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, file_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let file = service::note_file::read_for(&st.db, &NoteFileId::from_key(&file_id), note.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let bytes = tokio::fs::read(blob_path(&st.files_path, file.get_id().key()))
        .await
        .map_err(|err| {
            // The row exists but its blob doesn't — that's server-side damage
            // (a lost volume path), not a client 404.
            AppError::Internal(format!(
                "missing blob for note file {}: {err}",
                file.get_id().key()
            ))
        })?;
    // The stored content type is validated ASCII without control characters,
    // so it always parses; the fallback is for belt and braces.
    let content_type = HeaderValue::from_str(file.get_content_type().as_str())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let disposition = HeaderValue::from_str(&content_disposition(file.get_name().as_str()))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    Ok((
        [
            (CONTENT_TYPE, content_type),
            (CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}

/// Delete a note file (row first, then its blob).
#[utoipa::path(
    delete,
    path = "/{id}/files/{file_id}",
    tag = "notes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Note id"),
        ("file_id" = String, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Note or file not found", body = ErrorResponse),
    ),
)]
async fn delete_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, file_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let note = service::note::read_owned(&st.db, &NoteId::from_key(&id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let file = service::note_file::read_for(&st.db, &NoteFileId::from_key(&file_id), note.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let file = service::note_file::delete(&st.db, file).await?;
    remove_blob(&st.files_path, file.get_id().key()).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `Content-Disposition` for a download: an ASCII-safe `filename` fallback
/// plus the RFC 5987 `filename*` form so non-ASCII names (`ödev.pdf`) survive.
/// `attachment`, so an uploaded HTML/SVG never renders inline — shared with
/// homework submission files, which take any content type.
pub(crate) fn content_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        rfc5987_encode(name)
    )
}

/// Percent-encode everything outside RFC 5987's `attr-char` set, on UTF-8
/// bytes.
fn rfc5987_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disposition_keeps_ascii_and_encodes_the_rest() {
        assert_eq!(
            content_disposition("plan.pdf"),
            "attachment; filename=\"plan.pdf\"; filename*=UTF-8''plan.pdf"
        );
        // Non-ASCII survives in the RFC 5987 form; the fallback degrades.
        assert_eq!(
            content_disposition("ödev 1.pdf"),
            "attachment; filename=\"_dev 1.pdf\"; filename*=UTF-8''%C3%B6dev%201.pdf"
        );
        // Quotes can't break out of the quoted fallback.
        assert_eq!(
            content_disposition("a\"b.pdf"),
            "attachment; filename=\"a_b.pdf\"; filename*=UTF-8''a%22b.pdf"
        );
    }
}
