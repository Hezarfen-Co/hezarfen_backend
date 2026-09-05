//! Course notes: a teacher-authored note attached to a course, with file
//! attachments. Mirrors [`super::notes`] (personal notes) handler for
//! handler, with authorization swapped for the course-management wall:
//! writes need [`super::courses::can_manage_course`] (creator, an assigned
//! teacher, or manager+), reads need [`super::courses::can_view_course`]
//! (management rights, or enrollment).

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::rag::spawn_index;
use crate::constant::{MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::database::Database;
use crate::domain::course::{Course, CourseId};
use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteId, CourseNoteTitle};
use crate::domain::course_note_file::{
    CourseNoteFile, CourseNoteFileId, FileContentType, FileName,
};
use crate::domain::rag_output::{RagOutput, RagOutputId};
use crate::domain::settings::Settings;
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course};
use super::notes::content_disposition;
use super::{
    CurrentUser, Page, PageParams, RequireTeacher, UploadFileForm, blob_path, read_upload,
    remove_blob,
};

pub fn routes() -> OpenApiRouter<AppState> {
    // Same body-cap split as `notes`' file routes: the server-wide hard
    // ceiling plus multipart framing headroom, with the school's actual
    // (usually smaller) limit enforced while the stream is read.
    let files = OpenApiRouter::new()
        .routes(routes!(upload_file, list_files))
        .routes(routes!(download_file, delete_file))
        .layer(DefaultBodyLimit::max(
            MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
        ));
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(get_one, update, delete_one))
        .routes(routes!(list_rag))
        .routes(routes!(delete_rag))
        .merge(files)
}

/// The note plus its course, or a 404 — every entity handler here gates on
/// the parent course, so they always travel together.
async fn note_with_course(id: &str, db: &Database) -> Result<(CourseNote, Course), AppError> {
    let note = CourseNote::read(&CourseNoteId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = Course::read(note.get_course(), db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((note, course))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourseNote {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    course: String,
    #[schema(example = "Chapter 3 recap", max_length = 200)]
    title: String,
    #[schema(
        example = "Covered quadratics; homework due Friday",
        max_length = 10000
    )]
    content: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateCourseNote {
    #[schema(max_length = 200)]
    title: Option<String>,
    #[schema(max_length = 10000)]
    content: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct CourseNoteResponse {
    id: String,
    course: String,
    title: String,
    content: String,
}

impl CourseNoteResponse {
    fn new(note: &CourseNote) -> Self {
        Self {
            id: note.get_id().key().to_string(),
            course: note.get_course().key().to_string(),
            title: note.get_title().as_str().to_string(),
            content: note.get_content().as_str().to_string(),
        }
    }
}

/// Create a note on a course. Requires teacher+ and management rights over
/// the course (its creator, an assigned teacher, or a manager/admin).
#[utoipa::path(
    post,
    path = "/",
    tag = "course-notes",
    security(("session_cookie" = [])),
    request_body = CreateCourseNote,
    responses(
        (status = 201, description = "Note created", body = CourseNoteResponse),
        (status = 400, description = "Invalid title or content", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateCourseNote>,
) -> Result<(StatusCode, Json<CourseNoteResponse>), AppError> {
    let course = Course::read(&CourseId::from_key(&req.course), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add a course note",
        ));
    }
    course.require_open(&st.db).await?;
    let title = CourseNoteTitle::try_new(&req.title)?;
    let content = CourseNoteContent::try_new(&req.content.unwrap_or_default())?;
    let note = CourseNote::create(course.get_id(), user.get_id(), title, content, &st.db).await?;
    // Indexing is a background bonus, never a condition of storing the note:
    // the dispatch runs in its own task, so a slow or absent AI service cannot
    // delay or fail this 201.
    spawn_index(&st, note.clone());
    Ok((StatusCode::CREATED, Json(CourseNoteResponse::new(&note))))
}

#[derive(Deserialize, IntoParams)]
struct CourseFilter {
    /// The course to list notes for (required).
    #[param(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    course: String,
}

/// List a course's notes, newest first. Visible to whoever can view the
/// course (its enrolled users, creator, assigned teachers, and
/// managers/admins). Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(CourseFilter, PageParams),
    responses(
        (status = 200, description = "A page of the course's notes (all of them when unpaged)", body = Page<CourseNoteResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the course, not its creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(filter): Query<CourseFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<CourseNoteResponse>>, AppError> {
    let course = Course::read(&CourseId::from_key(&filter.course), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course's notes",
        ));
    }
    let (limit, offset) = page.resolve()?;
    let (notes, total) =
        CourseNote::list_for_course(course.get_id(), limit, offset, &st.db).await?;
    let items = notes.iter().map(CourseNoteResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single course note by id. Visible to whoever can view its course.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course note id")),
    responses(
        (status = 200, description = "The note", body = CourseNoteResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the course, not its creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_one(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<CourseNoteResponse>, AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course note",
        ));
    }
    Ok(Json(CourseNoteResponse::new(&note)))
}

/// Update a course note's title and/or content. Omitted fields keep their
/// value. Requires teacher+ and management rights over the course.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course note id")),
    request_body = UpdateCourseNote,
    responses(
        (status = 200, description = "Updated note", body = CourseNoteResponse),
        (status = 400, description = "Invalid title or content", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateCourseNote>,
) -> Result<Json<CourseNoteResponse>, AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this course note",
        ));
    }
    course.require_open(&st.db).await?;

    let title = req
        .title
        .as_deref()
        .map(CourseNoteTitle::try_new)
        .transpose()?;
    let content = req
        .content
        .as_deref()
        .map(CourseNoteContent::try_new)
        .transpose()?;

    let updated = note.update(title, content, &st.db).await?;
    // The stored index describes the old text — refresh it.
    spawn_index(&st, updated.clone());
    Ok(Json(CourseNoteResponse::new(&updated)))
}

/// Delete a course note, along with its files. Requires teacher+ and
/// management rights over the course.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course note id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_one(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this course note",
        ));
    }
    course.require_open(&st.db).await?;
    // Derived rows first, outside the note's own cascade transaction: they are
    // disposable, so failing here leaves the note intact and the 500 truthful,
    // whereas dropping them after the note would strand every blob on an error
    // — and if the note delete then fails, the next change regenerates them.
    RagOutput::delete_for_note(note.get_id(), &st.db).await?;
    // Rows go first (the note delete cascades them), blobs after: a crash in
    // between strands at worst an unreachable blob, never a row whose blob is
    // already gone. The files to unlink come from the delete itself, not a
    // pre-read list — an upload that landed in between is in the cascade too.
    let (_, files) = note.delete(&st.db).await?;
    for file in &files {
        remove_blob(&st.files_path, file.get_id().key()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A course note file's stored metadata; the bytes themselves come from the
/// download endpoint.
#[derive(Serialize, ToSchema)]
struct CourseNoteFileResponse {
    id: String,
    /// The uploader's original filename.
    #[schema(example = "recap.pdf")]
    name: String,
    /// MIME type as declared on upload.
    #[schema(example = "application/pdf")]
    content_type: String,
    /// File size in bytes.
    #[schema(example = 24_576)]
    size: i64,
}

impl CourseNoteFileResponse {
    fn new(file: &CourseNoteFile) -> Self {
        Self {
            id: file.get_id().key().to_string(),
            name: file.get_name().as_str().to_string(),
            content_type: file.get_content_type().as_str().to_string(),
            size: file.get_size(),
        }
    }
}

/// Attach a file to a course note. `multipart/form-data` with the file under
/// a `file` field; its `filename` is required. At most 10 files per note,
/// each at most the school's `max_file_bytes` (settings, default 5 MiB).
/// Requires teacher+ and management rights over the course.
#[utoipa::path(
    post,
    path = "/{id}/files",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course note id")),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "File stored", body = CourseNoteFileResponse),
        (status = 400, description = "Missing file field, invalid filename or content type, or empty file", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Note not found", body = ErrorResponse),
        (status = 409, description = "The note already holds the maximum number of files, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 413, description = "File exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_file(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<CourseNoteFileResponse>), AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can add a file to this course note",
        ));
    }
    course.require_open(&st.db).await?;
    // The 10-file cap is enforced inside `CourseNoteFile::insert` (count and
    // create under one lock) — checking it here too would just race.
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();

    let upload = read_upload(&mut multipart, limit).await?;
    let name = FileName::try_new(&upload.name.unwrap_or_default())?;
    let content_type = FileContentType::try_new(&upload.content_type.unwrap_or_default())?;

    // Blob first, row second — a stored row always points at a real blob. If
    // the row insert fails, take the fresh blob back out.
    let file = CourseNoteFile::new(note.get_id(), name, content_type, upload.data.len() as i64);
    let path = blob_path(&st.files_path, file.get_id().key());
    tokio::fs::write(&path, &upload.data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the file blob: {err}")))?;
    match file.insert(&st.db).await {
        Ok(created) => {
            // The note now holds one more source than the stored index knows.
            spawn_index(&st, note);
            Ok((
                StatusCode::CREATED,
                Json(CourseNoteFileResponse::new(&created)),
            ))
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err)
        }
    }
}

/// List a course note's files (metadata only), newest first. Paged via
/// `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/files",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course note id"), PageParams),
    responses(
        (status = 200, description = "A page of the note's files", body = Page<CourseNoteFileResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the course, not its creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Note not found", body = ErrorResponse),
    ),
)]
async fn list_files(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<CourseNoteFileResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course note's files",
        ));
    }
    let (files, total) = CourseNoteFile::list_for(note.get_id(), limit, offset, &st.db).await?;
    let items = files.iter().map(CourseNoteFileResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Download a course note file's bytes. `Content-Type` is the one declared on
/// upload; `Content-Disposition` carries the original filename.
#[utoipa::path(
    get,
    path = "/{id}/files/{file_id}",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course note id"),
        ("file_id" = String, Path, description = "File id"),
    ),
    responses(
        (status = 200, description = "The file bytes", content_type = "application/octet-stream"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the course, not its creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Note or file not found", body = ErrorResponse),
    ),
)]
async fn download_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, file_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can download this course note's files",
        ));
    }
    let file =
        CourseNoteFile::read_for(&CourseNoteFileId::from_key(&file_id), note.get_id(), &st.db)
            .await?
            .ok_or(AppError::NotFound)?;
    let bytes = tokio::fs::read(blob_path(&st.files_path, file.get_id().key()))
        .await
        .map_err(|err| {
            // The row exists but its blob doesn't — that's server-side damage
            // (a lost volume path), not a client 404.
            AppError::Internal(format!(
                "missing blob for course note file {}: {err}",
                file.get_id().key()
            ))
        })?;
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

/// Delete a course note file (row first, then its blob). Requires teacher+
/// and management rights over the course.
#[utoipa::path(
    delete,
    path = "/{id}/files/{file_id}",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course note id"),
        ("file_id" = String, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Note or file not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_file(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, file_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this course note's files",
        ));
    }
    course.require_open(&st.db).await?;
    let file =
        CourseNoteFile::read_for(&CourseNoteFileId::from_key(&file_id), note.get_id(), &st.db)
            .await?
            .ok_or(AppError::NotFound)?;
    // Drop every output built from this file before the file itself, so a
    // stale index cannot outlive its source in a deployment with no AI service
    // at all, and a failure here leaves the file whole instead of stranding its
    // blob; the re-index rebuilds from what is left, if a service is connected.
    RagOutput::delete_with_source(file.get_id(), &st.db).await?;
    let file = file.delete(&st.db).await?;
    remove_blob(&st.files_path, file.get_id().key()).await;
    spawn_index(&st, note);
    Ok(StatusCode::NO_CONTENT)
}

/// One AI service output stored against a course note. `payload` is the
/// service's own shape — the backend stores and serves it unread.
#[derive(Serialize, ToSchema)]
struct RagOutputResponse {
    id: String,
    course_note: String,
    course: String,
    /// The note's file attachments the output was built from.
    sources: Vec<String>,
    /// The service's answer, verbatim.
    #[schema(value_type = Object)]
    payload: serde_json::Value,
    /// When the backend stored it, epoch milliseconds.
    #[schema(example = 1_735_689_600_000i64)]
    generated_at: i64,
}

impl RagOutputResponse {
    fn new(output: &RagOutput) -> Self {
        Self {
            id: output.get_id().key().to_string(),
            course_note: output.get_course_note().key().to_string(),
            course: output.get_course().key().to_string(),
            sources: output
                .get_sources()
                .iter()
                .map(|file| file.key().to_string())
                .collect(),
            payload: output.get_payload().clone(),
            generated_at: output.get_generated_at().as_millis(),
        }
    }
}

/// List a course note's AI outputs, newest first. Visible to whoever can view
/// the course. Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/rag",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course note id"), PageParams),
    responses(
        (status = 200, description = "A page of the note's stored AI outputs", body = Page<RagOutputResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the course, not its creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Note not found", body = ErrorResponse),
    ),
)]
async fn list_rag(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<RagOutputResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this course note's AI outputs",
        ));
    }
    let (outputs, total) = RagOutput::list_for(note.get_id(), limit, offset, &st.db).await?;
    let items = outputs.iter().map(RagOutputResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Delete one stored AI output. Requires teacher+ and management rights over
/// the course. Dropping an output does not stop the next note or file change
/// from regenerating one.
#[utoipa::path(
    delete,
    path = "/{id}/rag/{output_id}",
    tag = "course-notes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course note id"),
        ("output_id" = String, Path, description = "AI output id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 404, description = "Note or output not found", body = ErrorResponse),
    ),
)]
async fn delete_rag(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, output_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let (note, course) = note_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this course note's AI outputs",
        ));
    }
    course.require_open(&st.db).await?;
    // Scoped to the note in the path, like `CourseNoteFile::read_for`: an
    // output of another note is a 404 here, never a cross-note delete.
    let output = RagOutput::read(&RagOutputId::from_key(&output_id), &st.db)
        .await?
        .filter(|output| output.get_course_note() == note.get_id())
        .ok_or(AppError::NotFound)?;
    RagOutput::delete(output.get_id(), &st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
