use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::course::Course;
use crate::domain::subject::{Subject, SubjectDescription, SubjectId, SubjectName};
use crate::error::{AppError, ErrorResponse};
use crate::service::course::{can_manage_course, can_view_course};
use crate::service::subject;
use crate::state::AppState;

use super::{CurrentUser, RequireTeacher, SubjectResponse};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_subject, update_subject, delete_subject))
}

#[derive(Deserialize, ToSchema)]
struct UpdateSubject {
    #[schema(max_length = 200)]
    name: Option<String>,
    #[schema(max_length = 2000)]
    description: Option<String>,
}

/// The subject plus its course, or a 404 — every handler here gates on the
/// parent course, so they always travel together.
async fn subject_with_course(id: &str, db: &Database) -> Result<(Subject, Course), AppError> {
    let subject = subject::read(db, &SubjectId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = crate::service::course::read(db, subject.get_course())
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((subject, course))
}

/// Fetch a single subject by id. Visible to whoever can view its course: its
/// creator, a manager/admin, or anyone the course reaches.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Subject id")),
    responses(
        (status = 200, description = "The subject", body = SubjectResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not reached by the course, and not its creator or a manager/admin", body = ErrorResponse),
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
            "only a user this course reaches, its creator, or a manager/admin can view this subject",
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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
            "only the course creator or a manager/admin can edit this subject",
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

    let updated = subject::update(&st.db, subject, name, description).await?;
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator or a manager/admin can delete this subject",
        ));
    }
    // No locks: the two checks *are* the delete's `WHERE`, decided against the
    // subject's own reference counters inside one statement. This used to be
    // three process-wide locks (the only site that held more than one) around
    // two cross-table counts, which were stale by the time the delete landed and
    // let a concurrent request create a question on a subject being deleted.
    subject::delete(&st.db, subject).await?;
    Ok(StatusCode::NO_CONTENT)
}
