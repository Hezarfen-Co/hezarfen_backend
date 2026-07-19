//! HTTP layer: axum routers, request/response DTOs (serde), and the auth
//! extractors. DTOs are plain strings on the wire; handlers parse them into
//! validated domain newtypes before doing anything. Role-gated endpoints take a
//! `RequireTeacher` / `RequireAdmin` extractor instead of `CurrentUser`.

pub mod attendance;
pub mod auth;
pub mod courses;
pub mod events;
pub mod exam_ws;
pub mod exams;
pub mod marks;
pub mod notes;
pub mod pomodoro;
pub mod sessions;
pub mod settings;
pub mod subjects;
pub mod terms;
pub mod users;
pub mod work;

mod dto;
mod extractor;
mod page;

pub use dto::{
    CourseResponse, ExamResponse, PersonRef, SessionResponse, SubjectResponse, UserResponse,
    person_map,
};
pub use extractor::{CurrentUser, RequireAdmin, RequireManager, RequireTeacher};
pub use page::{Page, PageParams, paginate};

use std::path::{Path as FsPath, PathBuf};

use axum::extract::Multipart;
use axum::extract::multipart::MultipartError;
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer};
use utoipa::ToSchema;

use crate::constant::SCHEDULE_PAST_GRACE_MS;
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};

/// If both ends are present, `ends_at` must not precede `starts_at`. Shared by
/// everything that carries a time range (events, course sessions).
pub(crate) fn check_time_range(
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<(), AppError> {
    if let (Some(starts), Some(ends)) = (starts_at, ends_at)
        && ends < starts
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "ends_at",
            reason: "must be at or after starts_at",
        }));
    }
    Ok(())
}

/// A schedule instant a request explicitly sets must not lie in the past —
/// nothing can be scheduled to start (or end) before now. Applies only to
/// values the request itself carries: stored values merged into a PATCH are
/// exempt, because a running exam's `starts_at` is legitimately past and a
/// title-only edit must not trip over it. `SCHEDULE_PAST_GRACE_MS` keeps
/// "starts now" from failing by the width of its own round trip.
pub(crate) fn check_not_past(
    field: &'static str,
    value: Option<Timestamp>,
) -> Result<(), AppError> {
    if let Some(value) = value
        && value.as_millis() < Timestamp::now().as_millis() - SCHEDULE_PAST_GRACE_MS
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field,
            reason: "must not be in the past",
        }));
    }
    Ok(())
}

// ---- uploaded blobs ---------------------------------------------------------
// Note files and question images share one story: bytes on disk under the
// configured directory in a file named by a server-generated ULID (user input
// never shapes a path), metadata in a row. Upload writes the blob before the
// row; delete removes the row before the blob — so a stored row always points
// at a real blob and a crash strands at worst an unreachable file.

/// Where a stored row's blob lives: one file under the configured directory,
/// named by the row's server-generated key.
pub(crate) fn blob_path(files_path: &FsPath, key: &str) -> PathBuf {
    files_path.join(key)
}

/// Best-effort blob removal after its row is gone. Failure only strands an
/// unreachable file on disk, so it is logged rather than surfaced.
pub(crate) async fn remove_blob(files_path: &FsPath, key: &str) {
    let path = blob_path(files_path, key);
    if let Err(err) = tokio::fs::remove_file(&path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("failed to remove blob {}: {err}", path.display());
    }
}

/// Multipart read failures: the route-level body cap maps to 413 like the
/// school-limit check; anything else is a malformed body.
pub(crate) fn multipart_error(err: MultipartError) -> AppError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::PayloadTooLarge("the upload exceeds the server's absolute body cap".to_string())
    } else {
        AppError::Validation(ValidationError::Invalid {
            field: "file",
            reason: "malformed multipart body",
        })
    }
}

/// Schema-only mirror of an upload form; the handlers read the multipart
/// stream directly.
#[derive(ToSchema)]
#[allow(dead_code)]
pub(crate) struct UploadFileForm {
    /// The file part. Its `Content-Type` (and, where required, `filename`)
    /// are stored alongside the bytes.
    #[schema(value_type = String, format = Binary)]
    file: String,
}

/// One uploaded file's raw parts, as the client sent them — the caller
/// validates what it cares about (notes need a filename, images an
/// allowlisted content type).
pub(crate) struct UploadedFile {
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// Pull the `file` field out of a multipart body, reading it chunkwise so an
/// oversized upload dies at `limit`, not after buffering whole — the check is
/// on real bytes received, so a lying `Content-Length` can't sneak past it.
/// Empty files are refused; stray extra fields are ignored.
pub(crate) async fn read_upload(
    multipart: &mut Multipart,
    limit: i64,
) -> Result<UploadedFile, AppError> {
    let mut field = loop {
        match multipart.next_field().await.map_err(multipart_error)? {
            Some(field) if field.name() == Some("file") => break field,
            Some(_) => continue,
            None => {
                return Err(AppError::Validation(ValidationError::Invalid {
                    field: "file",
                    reason: "the multipart body must carry a 'file' field",
                }));
            }
        }
    };
    let name = field.file_name().map(str::to_string);
    let content_type = field.content_type().map(str::to_string);

    let mut data = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if data.len() + chunk.len() > limit as usize {
            return Err(AppError::PayloadTooLarge(format!(
                "the file exceeds the school's limit of {limit} bytes"
            )));
        }
        data.extend_from_slice(&chunk);
    }
    if data.is_empty() {
        return Err(AppError::Validation(ValidationError::Empty("file")));
    }
    Ok(UploadedFile {
        name,
        content_type,
        data,
    })
}

/// Distinguishes an *absent* PATCH field from an explicit `null`: absent never
/// reaches the deserializer and stays `None` via `#[serde(default)]` (keep the
/// stored value); `null` or a value lands here as `Some(inner)` (clear or set).
pub(crate) fn set_or_clear<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_time_range_orders_the_ends() {
        let at = |ms| Some(Timestamp::from_millis(ms));
        assert!(check_time_range(at(1), at(2)).is_ok());
        assert!(check_time_range(at(1), at(1)).is_ok());
        assert!(check_time_range(at(2), at(1)).is_err());
        // A missing end never fails the range check.
        assert!(check_time_range(None, at(1)).is_ok());
        assert!(check_time_range(at(1), None).is_ok());
        assert!(check_time_range(None, None).is_ok());
    }

    #[tokio::test]
    async fn check_not_past_rejects_backdating_but_keeps_the_grace() {
        let now = Timestamp::now().as_millis();
        // Absent and future values pass.
        assert!(check_not_past("starts_at", None).is_ok());
        assert!(check_not_past("starts_at", Some(Timestamp::from_millis(now + 60_000))).is_ok());
        // Inside the grace passes — "starts now" survives its round trip.
        assert!(
            check_not_past(
                "starts_at",
                Some(Timestamp::from_millis(now - SCHEDULE_PAST_GRACE_MS / 2)),
            )
            .is_ok()
        );
        // Beyond the grace is a validation error naming the field.
        let past = Timestamp::from_millis(now - SCHEDULE_PAST_GRACE_MS - 60_000);
        assert!(matches!(
            check_not_past("ends_at", Some(past)),
            Err(AppError::Validation(ValidationError::Invalid {
                field: "ends_at",
                ..
            }))
        ));
    }
}
