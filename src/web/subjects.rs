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
use crate::domain::subject::{Subject, SubjectDescription, SubjectId, SubjectName};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course};
use super::exams::EXAM_LOCK;
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

    let name = match req.name {
        Some(ref name) => SubjectName::try_new(name)?,
        None => subject.get_name().clone(),
    };
    let description = match req.description {
        Some(ref description) => SubjectDescription::try_new(description)?,
        None => subject.get_description().clone(),
    };

    let updated = subject.update(name, description, &st.db).await?;
    Ok(Json(SubjectResponse::new(&updated)))
}

/// Delete a subject. Requires teacher+ and management rights over its course.
/// Refused with a 409 while any exam question still references it — re-tag or
/// delete those questions first, so no question is left without a real subject.
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
        (status = 409, description = "Exam questions still reference this subject", body = ErrorResponse),
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
    subject.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
