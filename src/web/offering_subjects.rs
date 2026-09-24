//! Syllabus topic selection on the course-template system: the topics an
//! offering ([`crate::domain::course_offering::CourseOffering`]) selects for
//! its grade, and the per-section override
//! ([`crate::domain::class_course::ClassCourse`]). The override rule is
//! override-or-inherit, never merge, and it is **flag-driven**:
//! `subjects_inherited` `TRUE` follows the offering's set, `FALSE` makes the
//! section's own table authoritative *including when it is empty* — the
//! section-level deletes and the reset door flip it in the same statement as
//! the row change ([`crate::db::offering_subject`]).
//!
//! This module exports two routers: [`offering_routes`] (mounted under
//! `/offerings` by the offerings router) and [`instance_routes`] (mounted
//! inside `/instances` like the exam/session/homework children). Reads are
//! open to any signed-in session; offering writes are the office's
//! (`manager`+); instance writes pay the D10 gate every other
//! instance-scoped write pays (`service::class_course::ensure_instance_teacher`)
//! and refuse on an archived year.
//!
//! Selection always names a subject of the offering's own course — a foreign
//! topic is the same 400 the question/homework tag check answers.

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_offering::{CourseOffering, CourseOfferingId};
use crate::domain::user::User;
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::state::AppState;
use crate::web::dto::SubjectResponse;
use crate::web::tenant_state::State;

use super::{CurrentUser, RequireManager, RequireTeacher};

/// The offering-side half: mounted under `/offerings`.
pub fn offering_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_offering_subjects, add_offering_subject))
        .routes(routes!(remove_offering_subject))
}

/// The instance-side half: mounted inside `/instances`, next to the exam,
/// session and homework children. The two `DELETE`s stay in separate
/// `routes!` calls — one call may register each HTTP method only once.
pub fn instance_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_instance_subjects, add_instance_subject))
        .routes(routes!(remove_instance_subject))
        .routes(routes!(reset_instance_subjects))
}

#[derive(Deserialize, ToSchema)]
struct AddSubject {
    /// The subject id to select (`GET /courses/{id}/subjects`). Must belong
    /// to the offering's own course — a foreign topic is a 400.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    subject: String,
}

/// The topics one section teaches, resolved override-or-inherit — plus the
/// switch, so a client can tell "the template's set" from "its own (possibly
/// empty) set".
#[derive(Serialize, ToSchema)]
struct ResolvedSubjects {
    /// The resolved set, by subject name then id.
    subjects: Vec<SubjectResponse>,
    /// `true` = the section follows its offering's set; `false` = its own
    /// selection is authoritative, even when the list above is empty.
    subjects_inherited: bool,
}

async fn offering_or_404(key: &str, db: &Database) -> Result<CourseOffering, AppError> {
    service::course_offering::read(db, &CourseOfferingId::from_key(key))
        .await?
        .ok_or(AppError::NotFound)
}

async fn instance_or_404(key: &str, db: &Database) -> Result<ClassCourse, AppError> {
    service::class_course::read(db, &ClassCourseId::from_key(key))
        .await?
        .ok_or(AppError::NotFound)
}

/// The instance-side write gate: D10 (manager+, this instance's teachers, its
/// şube's homeroom teacher) over a live year — the same pair every other
/// instance-scoped write pays.
async fn gated_instance(key: &str, user: &User, db: &Database) -> Result<ClassCourse, AppError> {
    let instance = instance_or_404(key, db).await?;
    service::class_course::ensure_instance_teacher(db, user, instance.get_id()).await?;
    service::class_course::require_open(db, instance.get_id()).await?;
    Ok(instance)
}

/// The offering's selected topics — the set every inheriting section at that
/// grade teaches — by subject name then id. Any signed-in session.
#[utoipa::path(
    get,
    path = "/{id}/subjects",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    responses(
        (status = 200, description = "The offering's selected subjects", body = [SubjectResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Offering not found", body = ErrorResponse),
    ),
)]
async fn list_offering_subjects(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<SubjectResponse>>, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    let subjects = service::offering_subject::resolved_for_offering(&st.db, offering.get_id())
        .await?;
    Ok(Json(subjects.iter().map(SubjectResponse::new).collect()))
}

/// Select a topic for the offering. Manager+. Re-selecting an already
/// selected topic is a no-op success (`200`, not a second row).
#[utoipa::path(
    post,
    path = "/{id}/subjects",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    request_body = AddSubject,
    responses(
        (status = 201, description = "Subject selected", body = SubjectResponse),
        (status = 200, description = "Already selected — no-op success", body = SubjectResponse),
        (status = 400, description = "Unknown subject, or one from another course", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Offering not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: `subject` is missing or malformed"),
    ),
)]
async fn add_offering_subject(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AddSubject>,
) -> Result<(StatusCode, Json<SubjectResponse>), AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    let (subject, added) =
        service::offering_subject::add_to_offering(&st.db, offering.get_id(), &req.subject)
            .await?;
    let code = if added {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((code, Json(SubjectResponse::new(&subject))))
}

/// Drop a topic from the offering's selection. Manager+. Dropping a topic the
/// offering never selected is a 404.
#[utoipa::path(
    delete,
    path = "/{id}/subjects/{subject}",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Offering id"),
        ("subject" = String, Path, description = "Subject id"),
    ),
    responses(
        (status = 204, description = "Dropped"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Offering not found, or subject not selected", body = ErrorResponse),
    ),
)]
async fn remove_offering_subject(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path((id, subject)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    service::offering_subject::remove_from_offering(&st.db, offering.get_id(), &subject).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The topics this section teaches, resolved override-or-inherit, with the
/// switch: `subjects_inherited: false` means the listed set (even when
/// empty) is the section's own and the offering's is ignored. Any signed-in
/// session.
#[utoipa::path(
    get,
    path = "/{id}/subjects",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance (class×course) id")),
    responses(
        (status = 200, description = "The resolved subject set and whether it is inherited", body = ResolvedSubjects),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn list_instance_subjects(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<ResolvedSubjects>, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    let subjects = service::offering_subject::resolved_for_instance(&st.db, &instance).await?;
    Ok(Json(ResolvedSubjects {
        subjects: subjects.iter().map(SubjectResponse::new).collect(),
        subjects_inherited: instance.subjects_inherited(),
    }))
}

/// Select a topic for this section's own set — the override. The switch
/// flips to "own" in the same write, so the section's table (not the
/// offering's set) becomes authoritative from this call on. Re-selecting an
/// already selected topic is a no-op success. D10 gate; refuses on an
/// archived year.
#[utoipa::path(
    post,
    path = "/{id}/subjects",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance (class×course) id")),
    request_body = AddSubject,
    responses(
        (status = 201, description = "Subject selected — the section now owns its set", body = SubjectResponse),
        (status = 200, description = "Already selected — no-op success", body = SubjectResponse),
        (status = 400, description = "Unknown subject, or one from another course", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: `subject` is missing or malformed"),
    ),
)]
async fn add_instance_subject(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<AddSubject>,
) -> Result<(StatusCode, Json<SubjectResponse>), AppError> {
    let instance = gated_instance(&id, &user, &st.db).await?;
    let (subject, added) =
        service::offering_subject::add_to_instance(&st.db, &instance, &req.subject).await?;
    let code = if added {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((code, Json(SubjectResponse::new(&subject))))
}

/// Drop one topic from this section's own set (keeping the set its own —
/// clearing the syllabus needs the reset below, or deleting the last rows
/// one by one). Dropping a topic the section never selected is a 404 and
/// flips nothing. D10 gate; refuses on an archived year.
#[utoipa::path(
    delete,
    path = "/{id}/subjects/{subject}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Instance (class×course) id"),
        ("subject" = String, Path, description = "Subject id"),
    ),
    responses(
        (status = 204, description = "Dropped — the section keeps its own set"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found, or subject not in the section's own set", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn remove_instance_subject(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, subject)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let instance = gated_instance(&id, &user, &st.db).await?;
    service::offering_subject::remove_from_instance(&st.db, &instance, &subject).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Restore inheritance: sweep the section's own rows and flip
/// `subjects_inherited` back to TRUE in the same statement — the offering's
/// set applies again. A section whose own set was already empty resets
/// cleanly. D10 gate; refuses on an archived year.
#[utoipa::path(
    delete,
    path = "/{id}/subjects",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance (class×course) id")),
    responses(
        (status = 204, description = "Own set swept — the offering's set applies again"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 409, description = "This instance's academic year is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn reset_instance_subjects(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let instance = gated_instance(&id, &user, &st.db).await?;
    service::offering_subject::reset_instance(&st.db, &instance).await?;
    Ok(StatusCode::NO_CONTENT)
}
