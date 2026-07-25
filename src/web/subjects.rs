use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::course::{Course, CourseId};
use crate::domain::exam_question::ExamQuestion;
use crate::domain::homework::Homework;
use crate::domain::subject::{Subject, SubjectDescription, SubjectId, SubjectName};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::bank_questions::BANK_LOCK;
use super::courses::{can_manage_course, can_view_course};
use super::exams::EXAM_LOCK;
use super::homework::HOMEWORK_LOCK;
use super::{CurrentUser, RequireTeacher, SubjectResponse};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_subject, update_subject, delete_subject))
}

#[derive(Deserialize, ToSchema)]
struct UpdateSubject {
    name: Option<String>,
    description: Option<String>,
}

/// Turn a request-supplied subject id into a validated reference, provided the
/// subject belongs to `course` — a question may only be tagged with a subject
/// of its own exam's course. Unknown or foreign subjects are a `400` naming
/// the field. Shared by the question create/update handlers.
pub(crate) async fn subject_in_course(
    id: &str,
    course: &CourseId,
    db: &Database,
) -> Result<SubjectId, AppError> {
    let subject =
        Subject::read(&SubjectId::from_key(id), db)
            .await?
            .ok_or(AppError::Validation(ValidationError::Invalid {
                field: "subject_id",
                reason: "subject does not exist",
            }))?;
    if subject.get_course() != course {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "subject_id",
            reason: "subject belongs to a different course",
        }));
    }
    Ok(subject.get_id().clone())
}

/// Turn a request-supplied subject id into a validated reference, checking only
/// that the subject exists — no course tie. For the question bank, whose subject
/// is cross-course origin metadata: the same-course rule applies at instantiate
/// time, not here. An unknown subject is a `400` naming the field.
pub(crate) async fn subject_must_exist(id: &str, db: &Database) -> Result<SubjectId, AppError> {
    let subject =
        Subject::read(&SubjectId::from_key(id), db)
            .await?
            .ok_or(AppError::Validation(ValidationError::Invalid {
                field: "subject_id",
                reason: "subject does not exist",
            }))?;
    Ok(subject.get_id().clone())
}

/// The subject plus its course, or a 404 — every handler here gates on the
/// parent course, so they always travel together.
async fn subject_with_course(id: &str, db: &Database) -> Result<(Subject, Course), AppError> {
    let subject = Subject::read(&SubjectId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = Course::read(subject.get_course(), db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((subject, course))
}

/// Fetch a single subject by id. Visible to whoever can view its course: the
/// course's enrolled users, its creator, its assigned teachers, and
/// managers/admins.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Subject id")),
    responses(
        (status = 200, description = "The subject", body = SubjectResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not the course creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_subject(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<SubjectResponse>, AppError> {
    let (subject, course) = subject_with_course(&id, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this subject",
        ));
    }
    Ok(Json(SubjectResponse::new(&subject)))
}

/// Update a subject's name or description. Requires teacher+ and management
/// rights over its course. Omitted fields keep their value; the course link is
/// fixed at creation.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Subject id")),
    request_body = UpdateSubject,
    responses(
        (status = 200, description = "Updated subject", body = SubjectResponse),
        (status = 400, description = "Invalid name or description", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn update_subject(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateSubject>,
) -> Result<Json<SubjectResponse>, AppError> {
    let (subject, course) = subject_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this subject",
        ));
    }

    // Only what the request actually carried is validated and written — an
    // omitted field stays `None` so the save never re-sends this snapshot's
    // value over a concurrent PATCH of the other field.
    let name = req.name.as_deref().map(SubjectName::try_new).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(SubjectDescription::try_new)
        .transpose()?;

    let updated = subject.update(name, description, &st.db).await?;
    Ok(Json(SubjectResponse::new(&updated)))
}

/// Delete a subject. Requires teacher+ and management rights over its course.
/// Refused with a 409 while any exam question or homework still references it —
/// re-tag or delete those first, so nothing is left pointing at a subject that
/// no longer exists.
///
/// Bank templates are the exception: their `subject` is optional origin
/// metadata, so the delete simply clears it on every template that carried it
/// instead of refusing. Blocking there was unresolvable (only a template's
/// owner may re-tag it, so a manager could not clear their own 409) and leaked
/// the existence of other teachers' private templates.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Subject id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Exam questions or homework still reference this subject", body = ErrorResponse),
    ),
)]
async fn delete_subject(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let (subject, course) = subject_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this subject",
        ));
    }
    // Writer lease of [`EXAM_LOCK`]: the no-questions check and the delete
    // are one unit, so a question create/update that just validated this
    // subject can't land its row on a subject that vanished mid-flight.
    let _guard = EXAM_LOCK.write().await;
    if ExamQuestion::any_for_subject(subject.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "exam questions still reference this subject — re-tag or delete them first",
        ));
    }
    // The homework twin of the guard above, under [`HOMEWORK_LOCK`]'s writer
    // lease: homework create validates its subject under the read side and the
    // PATCH re-tag under the write side, so the no-homework check and the
    // delete can't straddle a row that just adopted this subject. This is the
    // only place both locks are held; the order is EXAM_LOCK, then
    // HOMEWORK_LOCK.
    let _homework_guard = HOMEWORK_LOCK.write().await;
    if Homework::any_for_subject(subject.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "homework still references this subject — re-tag or delete it first",
        ));
    }
    // Bank templates carry this subject as optional origin metadata, so they
    // don't block: `Subject::delete` clears it off them in the delete's own
    // transaction. Writer lease of [`BANK_LOCK`] all the same — bank
    // create/update validate their subject and write under the reader lease, so
    // without it a template could adopt this subject *after* the cascade ran
    // and outlive it. Held last; the order is EXAM_LOCK, then HOMEWORK_LOCK,
    // then BANK_LOCK.
    let _bank_guard = BANK_LOCK.write().await;
    subject.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
