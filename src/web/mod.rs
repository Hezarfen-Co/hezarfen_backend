//! HTTP layer: axum routers, request/response DTOs (serde), and the auth
//! extractors. DTOs are plain strings on the wire; handlers parse them into
//! validated domain newtypes before doing anything. Role-gated endpoints take a
//! `RequireTeacher` / `RequireAdmin` extractor instead of `CurrentUser`.

pub mod ai;
pub mod appointments;
pub mod attendance;
pub mod auth;
pub mod bank_questions;
pub mod board_ws;
pub mod boards;
pub mod builder;
pub mod chatbot;
pub mod classes;
pub mod course_notes;
pub mod courses;
pub mod etag;
pub mod events;
pub mod exam_ws;
pub mod exams;
pub mod homework;
pub mod limits;
pub mod marks;
pub mod meals;
pub mod messages;
pub mod module_gate;
pub mod modules;
pub mod notes;
pub mod payments;
pub mod pomodoro;
pub mod questions;
pub mod room;
pub mod sessions;
pub mod settings;
pub mod subjects;
pub mod terms;
pub mod users;
pub mod work;

pub mod tenant_state;

mod dto;
// `pub(crate)` for `AiPrincipal`: the AI bridge injects it as an extension on
// the requests it dispatches into the router (see `ai::server`).
pub(crate) mod extractor;
mod image;
mod page;

pub use dto::{
    CourseResponse, ExamResponse, HomeworkResponse, PersonRef, SessionResponse, SubjectResponse,
    UserResponse, course_people, person_map,
};
pub use extractor::{
    BUILDER_COOKIE_PREFIX, CurrentUser, RequireAdmin, RequireBuilder, RequireManager,
    RequireStudent, RequireTeacher,
};
pub(crate) use image::{ImageUpload, read_image_upload, store_blob};
pub use page::{Page, PageParams, Scheduled, WindowParams, paginate};

use std::path::{Path as FsPath, PathBuf};

use axum::extract::Multipart;
use axum::extract::multipart::MultipartError;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Deserializer};
use utoipa::ToSchema;

use crate::constant::{QUESTION_IMAGE_CONTENT_TYPES, SCHEDULE_PAST_GRACE_MS};
use crate::database::Database;
use crate::domain::exam_question::{Choice, ChoiceInput};
use crate::domain::note_file::FileContentType;
use crate::domain::parent_link::ParentLink;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// May `caller` read `target`'s per-student reports (marks, attendance,
/// pomodoro)? Teacher+ always may (per-endpoint narrowing is the caller's
/// business); a parent may exactly when a `parent_link` row ties them to the
/// target *and* the target still holds the student role. Everyone else —
/// students included — gets a 403 (self-reads go through the `/me` endpoints).
pub(crate) async fn ensure_can_observe(
    caller: &User,
    target: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if caller.get_role().at_least(Role::Teacher) {
        return Ok(());
    }
    // The link row alone is not the grant: like stale enrollments, a link
    // whose student side changed role (a sweep lost a race with link_student)
    // must be inert, so the target's live role is re-read here. Missing or
    // non-student targets fall through to the same 403 — a parent never gets
    // an existence oracle.
    if caller.get_role() == Role::Parent
        && ParentLink::exists(caller.get_id(), target, db).await?
        && crate::service::user::read(db, target)
            .await?
            .is_some_and(|target| target.get_role() == Role::Student)
    {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "requires teacher role or higher, or a parent link to this student",
    ))
}

/// Close the window between "this account is teacher+" and the write that
/// records a teacher-only assignment (a class's homeroom teacher, a course's
/// teacher list). Call it *after* the write, with the account it named.
///
/// WHY: the demotion sweeps in `set_role` run **once**, over the rows that
/// exist at that moment. A `PATCH /users/{id}/role` interleaving between a
/// handler's role check and its write sweeps nothing — and nothing ever
/// re-sweeps — so the assignment stands forever, naming an account that may no
/// longer be a teacher at all. Re-reading the live role after the write is what
/// makes the pair ordered: either the sweep sees the row, or this read sees the
/// demotion.
///
/// The undo is the very sweep `set_role` owed, and both halves of it are
/// conditional writes scoped to *this* user (`teacher = $usr`,
/// `$usr IN teachers`), so a legitimate concurrent assignment of somebody else
/// is untouched — no lock, the repo's existing idiom. Both are run, not just
/// the caller's own: a demoted account may hold neither, and the row this
/// request did not write is exactly the one a lost sweep left behind.
///
/// A missing user counts as demoted. The ordinary path is one extra read and no
/// write at all.
pub(crate) async fn undo_if_demoted(target: &UserId, db: &Database) -> Result<(), AppError> {
    if crate::service::user::read(db, target)
        .await?
        .is_some_and(|user| user.get_role().at_least(Role::Teacher))
    {
        return Ok(());
    }
    crate::service::class_group::unassign_everywhere(db, target).await?;
    crate::service::course::unassign_everywhere(db, target).await?;
    Err(AppError::Conflict(
        "that user was demoted below teacher while this request ran — the assignment was undone; re-read their role and retry",
    ))
}

/// If both ends are present, `ends_at` must not precede `starts_at`. Shared by
/// everything that carries a time range (events, course sessions). The PATCH
/// paths re-check this at write time via
/// [`crate::db::field_update::FieldUpdate::ordered`], which answers with the
/// same error — this is the pre-flight, that is the race closer.
pub(crate) fn check_time_range(
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<(), AppError> {
    if let (Some(starts), Some(ends)) = (starts_at, ends_at)
        && ends < starts
    {
        return Err(crate::domain::timestamp::range_error());
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

/// Make sure a school's blob directory exists before writing into it.
///
/// `FILES_PATH` is the deployment root and `main` creates only that; each
/// school's subdirectory appears the first time that school stores a file, so
/// nothing has to be provisioned when a school is created.
pub(crate) async fn ensure_files_dir(files_path: &FsPath) -> Result<(), AppError> {
    tokio::fs::create_dir_all(files_path).await.map_err(|err| {
        AppError::Internal(format!(
            "failed to create the files directory {}: {err}",
            files_path.display()
        ))
    })
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

/// The declared content type of an inline-displayed image upload, held to the
/// raster allowlist — SVG stays out (it can script) since these bytes are
/// rendered inline to whole classes. Shared by exam question images and pool
/// question photos.
pub(crate) fn image_content_type(raw: &str) -> Result<FileContentType, AppError> {
    let content_type = FileContentType::try_new(raw)?;
    if !QUESTION_IMAGE_CONTENT_TYPES.contains(&content_type.as_str()) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "content_type",
            reason: "must be image/png, image/jpeg, image/webp, or image/gif",
        }));
    }
    Ok(content_type)
}

/// A stored blob as an inline-displayable response: the declared (and
/// allowlisted) content type, `nosniff`, and `no-store` — moderated/exam
/// content has no business in shared caches, and a replaced image must not
/// linger.
pub(crate) async fn serve_inline_blob(
    files_path: &FsPath,
    file: &str,
    content_type: &FileContentType,
) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(blob_path(files_path, file))
        .await
        .map_err(|err| {
            // The row exists but its blob doesn't — server-side damage (a lost
            // volume path), not a client 404.
            AppError::Internal(format!("missing blob for image {file}: {err}"))
        })?;
    let content_type = HeaderValue::from_str(content_type.as_str())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    Ok((
        [
            (CONTENT_TYPE, content_type),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (CACHE_CONTROL, HeaderValue::from_static("private, no-store")),
        ],
        bytes,
    )
        .into_response())
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

/// One option of a choice question as a client sends it — the wire form of
/// [`crate::domain::exam_question::ChoiceInput`], shared by the exam-question
/// and bank-template routes.
///
/// `id` is the option's key within the payload: send back the `id` the server
/// returned to keep that option (and its picture) across an edit, or any fresh
/// string to introduce a new one. The server mints the stored id either way, so
/// nothing a client sends here ever reaches storage.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub(crate) struct ChoiceBody {
    #[serde(default)]
    pub id: Option<String>,
    #[schema(example = "42", max_length = 500)]
    pub text: String,
}

impl ChoiceBody {
    pub(crate) fn into_input(self) -> ChoiceInput {
        ChoiceInput {
            id: self.id,
            text: self.text,
        }
    }

    /// The whole submitted list, or `None` when the field was omitted/cleared.
    pub(crate) fn into_inputs(choices: Option<Vec<Self>>) -> Option<Vec<ChoiceInput>> {
        choices.map(|list| list.into_iter().map(Self::into_input).collect())
    }

    /// The stored choices as a payload that re-submits them unchanged — how a
    /// PATCH that omits `choices` keeps every option's identity.
    pub(crate) fn from_stored(choices: Option<&[Choice]>) -> Option<Vec<ChoiceInput>> {
        choices.map(|list| {
            list.iter()
                .map(|choice| ChoiceInput {
                    id: Some(choice.get_id().as_str().to_string()),
                    text: choice.get_text().as_str().to_string(),
                })
                .collect()
        })
    }
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

    /// The demotion-vs-assign race, driven in the order the race produces it:
    /// both assignments are already written (the handler's role check has
    /// passed), *then* the account drops below `teacher` with no sweep — which
    /// is exactly what `set_role`'s one-shot sweep leaves behind when it runs
    /// before the write. The post-write re-read must undo both columns and
    /// refuse; with the account still staff it must undo nothing at all.
    ///
    /// Asserted on stored state, never on a return value: the in-memory engine
    /// forges those (see [`crate::db::cap`]).
    #[tokio::test]
    async fn undo_if_demoted_repairs_both_assignments_or_neither() {
        use crate::domain::class_group::ClassName;
        use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
        use crate::domain::user::{Password, Username};

        let db = crate::database::init_mem().await.unwrap();
        let office = UserId::from_key("office");
        let teacher = crate::service::user::create(
            &db,
            Username::try_new("ada").unwrap(),
            Password::try_new("secret1")
                .unwrap()
                .hash_async()
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        crate::service::user::set_role(&db, teacher.get_id(), Role::Teacher)
            .await
            .unwrap()
            .0;
        let assigned = async |db: &Database| {
            let class = crate::service::class_group::create(
                db,
                &office,
                ClassName::try_new("9-A").unwrap(),
                None,
                None,
                Some(teacher.get_id().clone()),
            )
            .await
            .unwrap();
            let course = crate::service::course::assign_teacher(
                db,
                &crate::service::course::create(
                    db,
                    &office,
                    CourseTitle::try_new("algebra").unwrap(),
                    CourseDescription::try_new("").unwrap(),
                    CourseKind::course(),
                    None,
                    None,
                )
                .await
                .unwrap(),
                teacher.get_id(),
            )
            .await
            .unwrap();
            (class, course)
        };
        /// `(the class's stored teacher, the course's stored teacher list)`.
        async fn stored(
            class: &crate::domain::class_group::ClassGroupId,
            course: &crate::domain::course::CourseId,
            db: &Database,
        ) -> (Option<UserId>, Vec<UserId>) {
            (
                crate::service::class_group::read(db, class)
                    .await
                    .unwrap()
                    .unwrap()
                    .get_teacher()
                    .cloned(),
                crate::service::course::read(db, course)
                    .await
                    .unwrap()
                    .unwrap()
                    .get_teachers()
                    .to_vec(),
            )
        }

        // Still staff: one read, and not a single write.
        let (class, course) = assigned(&db).await;
        undo_if_demoted(teacher.get_id(), &db)
            .await
            .expect("an account that is still teacher+ keeps what it was given");
        assert_eq!(
            stored(class.get_id(), course.get_id(), &db).await,
            (
                Some(teacher.get_id().clone()),
                vec![teacher.get_id().clone()]
            ),
            "nothing may be undone while the bar still holds"
        );

        // Demoted with no sweep — the state a lost race leaves.
        db.query("UPDATE $usr SET role = 'student'")
            .bind(("usr", teacher.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let refused = undo_if_demoted(teacher.get_id(), &db).await;
        assert!(
            matches!(refused, Err(AppError::Conflict(message)) if message.contains("demoted")),
            "a demotion mid-request must be a 409 naming it: {refused:?}"
        );
        assert_eq!(
            stored(class.get_id(), course.get_id(), &db).await,
            (None, Vec::new()),
            "…with both assignments taken back, not just the caller's own"
        );

        // A user that is gone counts as demoted, and the repair is idempotent.
        assert!(
            undo_if_demoted(&UserId::from_key("ghost"), &db)
                .await
                .is_err()
        );
        assert!(undo_if_demoted(teacher.get_id(), &db).await.is_err());
        assert_eq!(
            stored(class.get_id(), course.get_id(), &db).await,
            (None, Vec::new())
        );
    }

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
