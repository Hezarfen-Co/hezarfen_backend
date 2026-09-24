//! The exam-weight override routes: per-kind weights on the grade-level
//! template (`/offerings/{id}/exam-weights`, manager+) and on the class
//! section (`/instances/{id}/exam-weights`, the D10 instance gate), plus the
//! section's reset.
//!
//! Every `GET` returns the **resolved** map — the effective weight of every
//! kind under that level, computed here — so a client renders it verbatim and
//! never re-implements the class → offering → settings → 1 chain. The
//! instance map carries the `inherited` flag: while it is `true` the map is
//! the offering's resolved set; while `false` it is exactly the section's own
//! rows, empty included (an own set in force is authoritative even when
//! empty).
//!
//! A write naming a kind the school does not run is the one refusal here with
//! a machine code — 400 `unknown_exam_kind` — because a client can branch on
//! "fix the kind" vs "fix the weight"; it is rendered by this module (the
//! service's `ValidationError::Invalid { field: "kind" }` re-wrapped), since
//! the coded-body vocabulary lives with the route that publishes it.

use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::exam_weight::ExamWeight;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;
use crate::web::instances::can_view_instance;
use crate::web::tenant_state::State;

use super::{CurrentUser, RequireManager, RequireTeacher};

/// Mounted under `/offerings` — the template's weight routes.
pub fn offering_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_offering_weights, patch_offering_weight))
        .routes(routes!(delete_offering_weight))
}

/// Mounted inside `/instances` — the section's weight routes and reset.
pub fn instance_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_instance_weights, patch_instance_weight))
        .routes(routes!(delete_instance_weight))
        .routes(routes!(reset_instance_weights))
}

#[derive(Deserialize, ToSchema)]
struct PatchExamWeight {
    /// The exam kind exactly as the school spells it in `GET /settings`
    /// (`settings.exam_kinds`). Anything else is a 400 `unknown_exam_kind`.
    #[schema(example = "midterm")]
    kind: String,
    /// The weight this kind carries under the level being edited: 1..=100.
    #[schema(minimum = 1, maximum = 100, example = 3)]
    weight: i64,
}

/// One resolved entry: the kind and the weight an exam of it carries *here*
/// after the whole fallback chain.
#[derive(Serialize, ToSchema)]
pub struct ExamWeightEntry {
    #[schema(example = "midterm")]
    pub kind: String,
    pub weight: i64,
}

impl ExamWeightEntry {
    pub(crate) fn of(row: &ExamWeight) -> Self {
        Self {
            kind: row.get_kind().to_string(),
            weight: row.get_weight(),
        }
    }
}

/// The template's resolved weight map.
#[derive(Serialize, ToSchema)]
pub struct OfferingExamWeightsResponse {
    /// Every kind the school runs (plus any kind a stored row adds), each at
    /// its effective weight: the offering's own row, else the settings
    /// weight, else 1.
    pub weights: Vec<ExamWeightEntry>,
}

/// The section's resolved weight map, with the override switch.
#[derive(Serialize, ToSchema)]
pub struct InstanceExamWeightsResponse {
    /// `true` = the section follows its offering's set (the map below is the
    /// offering's, resolved); `false` = the section's own table is
    /// authoritative and the map is exactly its rows — including when that is
    /// none, which is how a section zeroes the template's weights.
    pub inherited: bool,
    pub weights: Vec<ExamWeightEntry>,
}

async fn offering_or_404(key: &str, db: &Database) -> Result<CourseOfferingId, AppError> {
    let id = CourseOfferingId::from_key(key);
    if service::course_offering::read(db, &id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(id)
}

/// The instance row the write gates judge — a missing id is a 404 before any
/// body is read.
async fn instance_or_404(key: &str, db: &Database) -> Result<ClassCourse, AppError> {
    service::class_course::read(db, &ClassCourseId::from_key(key))
        .await?
        .ok_or(AppError::NotFound)
}

/// Re-renders the service's kind refusal as the coded 400 this route
/// documents; every other error keeps its own wire shape.
fn kind_refusal(err: AppError, kind: &str) -> Response {
    if let AppError::Validation(ValidationError::Invalid { field: "kind", .. }) = &err {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("kind: `{kind}` is not a known exam kind"),
                "code": "unknown_exam_kind",
            })),
        )
            .into_response();
    }
    err.into_response()
}

// ---- the template's weights --------------------------------------------------

/// The grade-level template's resolved exam-kind weights. Any signed-in
/// session.
#[utoipa::path(
    get,
    path = "/{id}/exam-weights",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    responses(
        (status = 200, description = "The offering's resolved weights", body = OfferingExamWeightsResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Offering not found", body = ErrorResponse),
    ),
)]
async fn get_offering_weights(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<OfferingExamWeightsResponse>, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    let weights = service::exam_weight::resolved_for_offering(&st.db, &offering).await?;
    Ok(Json(OfferingExamWeightsResponse {
        weights: weights.iter().map(ExamWeightEntry::of).collect(),
    }))
}

/// Set (or rewrite) one kind's weight on the template. Manager+. The kind must
/// be one the school runs — a 400 carrying the machine code
/// `unknown_exam_kind` otherwise; the weight must sit in 1..=100. Sections
/// that still inherit the template pick the change up immediately.
#[utoipa::path(
    patch,
    path = "/{id}/exam-weights",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Offering id")),
    request_body = PatchExamWeight,
    responses(
        (status = 200, description = "The offering's resolved weights after the write", body = OfferingExamWeightsResponse),
        (status = 400, description = "A weight outside 1..=100, or a kind the school does not run (body carries the machine code `unknown_exam_kind`)", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Offering not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn patch_offering_weight(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<PatchExamWeight>,
) -> Result<Response, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    if let Err(err) =
        service::exam_weight::set_for_offering(&st.db, &offering, &req.kind, req.weight).await
    {
        return Ok(kind_refusal(err, &req.kind));
    }
    let weights = service::exam_weight::resolved_for_offering(&st.db, &offering).await?;
    Ok(Json(OfferingExamWeightsResponse {
        weights: weights.iter().map(ExamWeightEntry::of).collect(),
    })
    .into_response())
}

/// Drop one kind's weight row from the template; the settings weight applies
/// again. A kind with no row is a 404.
#[utoipa::path(
    delete,
    path = "/{id}/exam-weights/{kind}",
    tag = "offerings",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Offering id"),
        ("kind" = String, Path, description = "Exam kind"),
    ),
    responses(
        (status = 204, description = "Row deleted — the settings weight applies again"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Offering not found, or it carries no weight row for this kind", body = ErrorResponse),
    ),
)]
async fn delete_offering_weight(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path((id, kind)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let offering = offering_or_404(&id, &st.db).await?;
    service::exam_weight::remove_from_offering(&st.db, &offering, &kind).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- the section's weights ---------------------------------------------------

/// The section's resolved exam-kind weights and its override switch. Visible
/// to the same eyes as the rest of the instance's reads.
#[utoipa::path(
    get,
    path = "/{id}/exam-weights",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    responses(
        (status = 200, description = "The section's resolved weights, with the inherited flag", body = InstanceExamWeightsResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled, not this instance's teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn get_instance_weights(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<InstanceExamWeightsResponse>, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    if !can_view_instance(&st.db, instance.get_id(), &user).await? {
        return Err(AppError::Forbidden(
            "only this instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view its weights",
        ));
    }
    let (inherited, weights) = service::exam_weight::resolved_for_class(&st.db, &instance).await?;
    Ok(Json(InstanceExamWeightsResponse {
        inherited,
        weights: weights.iter().map(ExamWeightEntry::of).collect(),
    }))
}

/// Set (or rewrite) one kind's weight on the section — and take its weight set
/// own: the flag flips in the same statement, so from here on the section's
/// own rows (these included) are the whole map, even where they are absent.
/// Requires teacher+ and a right over the instance (D10).
#[utoipa::path(
    patch,
    path = "/{id}/exam-weights",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    request_body = PatchExamWeight,
    responses(
        (status = 200, description = "The section's resolved weights after the write (`inherited` now `false`)", body = InstanceExamWeightsResponse),
        (status = 400, description = "A weight outside 1..=100, or a kind the school does not run (body carries the machine code `unknown_exam_kind`)", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn patch_instance_weight(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<PatchExamWeight>,
) -> Result<Response, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    if let Err(err) =
        service::exam_weight::set_for_class(&st.db, instance.get_id(), &req.kind, req.weight).await
    {
        return Ok(kind_refusal(err, &req.kind));
    }
    // The write flipped the flag, so the map is computed off the fresh row,
    // never the one the gate read.
    let fresh = service::class_course::read(&st.db, instance.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let (inherited, weights) = service::exam_weight::resolved_for_class(&st.db, &fresh).await?;
    Ok(Json(InstanceExamWeightsResponse {
        inherited,
        weights: weights.iter().map(ExamWeightEntry::of).collect(),
    })
    .into_response())
}

/// Drop one kind's weight row from the section. Deleting still takes the set
/// own — removing the last row *decides* on an empty set (every kind then
/// weighs 1 here) rather than falling back. A kind with no row is a 404 that
/// changes nothing.
#[utoipa::path(
    delete,
    path = "/{id}/exam-weights/{kind}",
    tag = "instances",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Instance id"),
        ("kind" = String, Path, description = "Exam kind"),
    ),
    responses(
        (status = 204, description = "Row deleted — the section's own set stays authoritative without it"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found, or it carries no weight row for this kind", body = ErrorResponse),
    ),
)]
async fn delete_instance_weight(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, kind)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::exam_weight::remove_from_class(&st.db, instance.get_id(), &kind).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The reset: drop every section weight row and follow the offering's set
/// (then the settings, then 1) again. Idempotent.
#[utoipa::path(
    delete,
    path = "/{id}/exam-weights",
    tag = "instances",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Instance id")),
    responses(
        (status = 204, description = "Rows deleted — inheritance restored"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Instance not found", body = ErrorResponse),
    ),
)]
async fn reset_instance_weights(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let instance = instance_or_404(&id, &st.db).await?;
    service::class_course::ensure_instance_teacher(&st.db, &user, instance.get_id()).await?;
    service::exam_weight::reset_class(&st.db, instance.get_id()).await?;
    Ok(StatusCode::NO_CONTENT)
}
