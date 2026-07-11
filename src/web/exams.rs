use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::exam::{Exam, ExamDescription, ExamId, ExamKind, ExamTitle};
use crate::domain::exam_result::{ExamResult, Mark};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{CurrentUser, RequireTeacher};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_exam, list_exams))
        .routes(routes!(get_exam, update_exam, delete_exam))
        .routes(routes!(grade, list_results))
        .routes(routes!(my_result))
        .routes(routes!(remove_result))
}

#[derive(Deserialize, ToSchema)]
struct CreateExam {
    #[schema(example = "Chapter 3 quiz")]
    title: String,
    description: Option<String>,
    /// The assessment form: `homework` or `quiz`.
    #[schema(example = "quiz")]
    kind: String,
}

#[derive(Deserialize, ToSchema)]
struct UpdateExam {
    title: Option<String>,
    description: Option<String>,
    kind: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct GradeResult {
    /// The mark to record, `0`–`100`.
    #[schema(example = 85)]
    mark: i64,
    /// The student being graded.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    user_id: String,
}

#[derive(Serialize, ToSchema)]
struct ExamResponse {
    id: String,
    creator: String,
    title: String,
    description: String,
    kind: String,
}

impl ExamResponse {
    fn new(exam: &Exam) -> Self {
        Self {
            id: exam.get_id().key().to_string(),
            creator: exam.get_creator().key().to_string(),
            title: exam.get_title().as_str().to_string(),
            description: exam.get_description().as_str().to_string(),
            kind: exam.get_kind().as_str().to_string(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ExamResultResponse {
    id: String,
    exam: String,
    user: String,
    mark: i64,
    graded_by: String,
}

impl ExamResultResponse {
    fn new(result: &ExamResult) -> Self {
        Self {
            id: result.get_id().key().to_string(),
            exam: result.get_exam().key().to_string(),
            user: result.get_user().key().to_string(),
            mark: result.get_mark().as_i64(),
            graded_by: result.get_graded_by().key().to_string(),
        }
    }
}

/// Who may edit/delete a specific exam: its creator, or anyone `manager` and
/// above. Callers have already cleared the `teacher` bar via `RequireTeacher`.
fn can_manage(exam: &Exam, user: &User) -> bool {
    exam.is_creator(user.get_id()) || user.get_role().at_least(Role::Manager)
}

// ---- exams --------------------------------------------------------------

/// Create an exam owned by the current user. Requires the `teacher` role or higher.
#[utoipa::path(
    post,
    path = "/",
    tag = "exams",
    security(("session_cookie" = [])),
    request_body = CreateExam,
    responses(
        (status = 201, description = "Exam created", body = ExamResponse),
        (status = 400, description = "Invalid fields or kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn create_exam(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateExam>,
) -> Result<(StatusCode, Json<ExamResponse>), AppError> {
    let title = ExamTitle::try_new(&req.title)?;
    let description = ExamDescription::try_new(&req.description.unwrap_or_default())?;
    let kind = ExamKind::try_new(&req.kind)?;
    let exam = Exam::create(user.get_id(), title, description, kind, &st.db).await?;
    Ok((StatusCode::CREATED, Json(ExamResponse::new(&exam))))
}

/// List all exams.
#[utoipa::path(
    get,
    path = "/",
    tag = "exams",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "All exams", body = [ExamResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_exams(
    State(st): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<ExamResponse>>, AppError> {
    let exams = Exam::list_all(&st.db).await?;
    Ok(Json(exams.iter().map(ExamResponse::new).collect()))
}

/// Fetch a single exam by id.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The exam", body = ExamResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_exam(
    State(st): State<AppState>,
    _user: CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<ExamResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(ExamResponse::new(&exam)))
}

/// Update an exam. Requires teacher+; the creator may edit their own exam and
/// managers/admins may edit anyone's. Omitted fields keep their value.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = UpdateExam,
    responses(
        (status = 200, description = "Updated exam", body = ExamResponse),
        (status = 400, description = "Invalid fields or kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn update_exam(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateExam>,
) -> Result<Json<ExamResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&exam, &user) {
        return Err(AppError::Forbidden(
            "only the creator or a manager/admin can edit this exam",
        ));
    }

    let title = match req.title {
        Some(ref title) => ExamTitle::try_new(title)?,
        None => exam.get_title().clone(),
    };
    let description = match req.description {
        Some(ref description) => ExamDescription::try_new(description)?,
        None => exam.get_description().clone(),
    };
    let kind = match req.kind {
        Some(ref kind) => ExamKind::try_new(kind)?,
        None => exam.get_kind().clone(),
    };

    let updated = exam.update(title, description, kind, &st.db).await?;
    Ok(Json(ExamResponse::new(&updated)))
}

/// Delete an exam. Requires teacher+; the creator may delete their own exam and
/// managers/admins may delete anyone's. Cascades the exam's result rows.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_exam(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage(&exam, &user) {
        return Err(AppError::Forbidden(
            "only the creator or a manager/admin can delete this exam",
        ));
    }
    exam.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- results ------------------------------------------------------------

/// Record (or overwrite) a student's mark for an exam. Requires teacher+.
/// Students never grade — including themselves.
#[utoipa::path(
    post,
    path = "/{id}/results",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = GradeResult,
    responses(
        (status = 200, description = "Result recorded", body = ExamResultResponse),
        (status = 400, description = "Invalid mark or unknown user", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or attempted to grade yourself", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn grade(
    State(st): State<AppState>,
    RequireTeacher(teacher): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<GradeResult>,
) -> Result<Json<ExamResultResponse>, AppError> {
    let exam_id = ExamId::from_key(&id);
    // Exam must exist.
    Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let mark = Mark::try_new(req.mark)?;
    let target = UserId::from_key(&req.user_id);

    // Grading never targets oneself — no grader, whatever their role, may
    // write their own mark.
    if &target == teacher.get_id() {
        return Err(AppError::Forbidden("grading yourself is not allowed"));
    }

    // Target user must exist.
    if User::read(&target, &st.db).await?.is_none() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    }

    let result = ExamResult::grade(&exam_id, &target, mark, teacher.get_id(), &st.db).await?;
    Ok(Json(ExamResultResponse::new(&result)))
}

/// List every result for an exam. Requires teacher+ — students read only their
/// own via `GET /exams/{id}/result`.
#[utoipa::path(
    get,
    path = "/{id}/results",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "All results", body = [ExamResultResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn list_results(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<Vec<ExamResultResponse>>, AppError> {
    let exam_id = ExamId::from_key(&id);
    // Exam must exist — a missing exam is a 404, not an empty result list.
    Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let results = ExamResult::list_for_exam(&exam_id, &st.db).await?;
    Ok(Json(results.iter().map(ExamResultResponse::new).collect()))
}

/// The current user's own result for an exam. Any authenticated user may read
/// their own mark; `404` while ungraded (or when the exam doesn't exist).
#[utoipa::path(
    get,
    path = "/{id}/result",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The caller's result", body = ExamResultResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such exam, or not graded yet", body = ErrorResponse),
    ),
)]
async fn my_result(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<ExamResultResponse>, AppError> {
    let result = ExamResult::read_for_user(&ExamId::from_key(&id), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(ExamResultResponse::new(&result)))
}

/// Remove a student's result from an exam. Requires teacher+.
#[utoipa::path(
    delete,
    path = "/{id}/results/{user}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn remove_result(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let removed =
        ExamResult::remove(&ExamId::from_key(&id), &UserId::from_key(&target), &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
