use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::{Stream, StreamExt};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::EXAM_LIVE_STREAM_INTERVAL_SECS;
use crate::database::Database;
use crate::domain::course::Course;
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{
    Exam, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode, ExamSchedule, ExamTitle,
    ExamWeight,
};
use crate::domain::exam_answer::{ExamAnswer, auto_score};
use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt};
use crate::domain::exam_question::{
    ExamQuestion, ExamQuestionId, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::exam_result::{ExamResult, Mark};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::can_manage_course;
use super::{CurrentUser, ExamResponse, PersonRef, RequireTeacher, person_map, set_or_clear};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        // Plain route: OpenApiRouter can't describe a WebSocket upgrade, so
        // the exam room lives outside the generated spec (see the `exams` tag
        // description and README for the protocol).
        .route(
            "/{id}/attempt/ws",
            axum::routing::get(super::exam_ws::attempt_ws),
        )
        .routes(routes!(list_exams))
        .routes(routes!(get_exam, update_exam, delete_exam))
        .routes(routes!(grade, list_results))
        .routes(routes!(my_result))
        .routes(routes!(remove_result))
        .routes(routes!(exam_statistics))
        .routes(routes!(start_attempt, my_attempt))
        .routes(routes!(finish_attempt))
        .routes(routes!(exam_live))
        .routes(routes!(exam_live_stream))
        .routes(routes!(create_question, list_questions))
        .routes(routes!(update_question, delete_question))
        .routes(routes!(attempt_questions))
        .routes(routes!(save_answer))
        .routes(routes!(attempt_answers))
}

#[derive(Deserialize, ToSchema)]
struct UpdateExam {
    title: Option<String>,
    description: Option<String>,
    kind: Option<String>,
    weight: Option<i64>,
    /// `sync` or `async`. Omit to keep the current mode; send `null` to
    /// unschedule the exam. Frozen once anyone has started an attempt.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    mode: Option<Option<String>>,
    /// Window open, UTC unix-milliseconds. Omit to keep; `null` to clear.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    starts_at: Option<Option<i64>>,
    /// Window close, UTC unix-milliseconds. Omit to keep; `null` to clear.
    /// Moving it while a sync exam runs extends (or cuts) everyone's deadline.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    ends_at: Option<Option<i64>>,
    /// Per-student budget, milliseconds (async only). Omit to keep; `null` to
    /// clear. Changing it mid-exam moves every running attempt's deadline.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    duration_ms: Option<Option<i64>>,
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
struct ExamResultResponse {
    id: String,
    exam: String,
    /// The graded student.
    user: PersonRef,
    mark: i64,
    /// Who recorded the mark.
    graded_by: PersonRef,
}

impl ExamResultResponse {
    fn new(result: &ExamResult, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: result.get_id().key().to_string(),
            exam: result.get_exam().key().to_string(),
            user: PersonRef::resolve(people, result.get_user()),
            mark: result.get_mark().as_i64(),
            graded_by: PersonRef::resolve(people, result.get_graded_by()),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ExamStatisticsResponse {
    exam: String,
    /// Number of graded results.
    graded: u64,
    /// Plain mean of the graded marks; `null` while nothing is graded.
    average: Option<f64>,
    min: Option<i64>,
    max: Option<i64>,
}

/// The course an exam belongs to. A dangling reference means the course-delete
/// cascade was violated — surface it loudly as a 500, not a user-facing 404.
async fn course_of(exam: &Exam, db: &Database) -> Result<Course, AppError> {
    Course::read(exam.get_course(), db)
        .await?
        .ok_or_else(|| AppError::Internal("exam references a missing course".into()))
}

// ---- exams --------------------------------------------------------------
// Exams are created inside a course: `POST /courses/{id}/exams`.

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

/// Update an exam. Requires teacher+ and management rights over the exam's
/// course (its creator, or manager/admin). Omitted fields keep their value; an
/// explicit `null` clears a schedule field; the course itself is not updatable.
/// The schedule must stay consistent as a whole (see the create endpoint), and
/// `mode` is frozen once anyone has started an attempt — times and duration
/// stay editable so a running exam can be extended live.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = UpdateExam,
    responses(
        (status = 200, description = "Updated exam", body = ExamResponse),
        (status = 400, description = "Invalid fields, kind, weight, or schedule", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Mode change after attempts started", body = ErrorResponse),
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
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can edit this exam",
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
    let weight = match req.weight {
        Some(weight) => ExamWeight::try_new(weight)?,
        None => exam.get_weight(),
    };

    // Merge the schedule (set / clear / keep per field), then re-validate it
    // as a unit — a PATCH can't leave a half-schedule behind.
    let mode = match req.mode {
        Some(update) => update.as_deref().map(ExamMode::try_new).transpose()?,
        None => exam.get_mode().cloned(),
    };
    let starts_at = match req.starts_at {
        Some(update) => update.map(Timestamp::from_millis),
        None => exam.get_starts_at(),
    };
    let ends_at = match req.ends_at {
        Some(update) => update.map(Timestamp::from_millis),
        None => exam.get_ends_at(),
    };
    let duration_ms = match req.duration_ms {
        Some(update) => update.map(ExamDuration::try_new).transpose()?,
        None => exam.get_duration_ms(),
    };
    let schedule = ExamSchedule::try_new(mode, starts_at, ends_at, duration_ms)?;

    // Switching sync <-> async (or unscheduling) would silently rewrite the
    // deadline rules under students who already sat down; extending times is
    // the supported live adjustment instead.
    let mode_changed =
        schedule.get_mode().map(ExamMode::as_str) != exam.get_mode().map(ExamMode::as_str);
    if mode_changed && ExamAttempt::any_for_exam(exam.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "cannot change the exam mode after attempts have started",
        ));
    }

    let updated = exam
        .update(title, description, kind, weight, schedule, &st.db)
        .await?;
    Ok(Json(ExamResponse::new(&updated)))
}

/// Delete an exam. Requires teacher+ and management rights over the exam's
/// course (its creator, or manager/admin). Cascades the exam's result and
/// attempt rows.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
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
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can delete this exam",
        ));
    }
    exam.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- results ------------------------------------------------------------

/// Record (or overwrite) a student's mark for an exam. Requires teacher+ and
/// management rights over the exam's course; the target must be enrolled.
/// Students never grade — and nobody grades themselves.
#[utoipa::path(
    post,
    path = "/{id}/results",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = GradeResult,
    responses(
        (status = 200, description = "Result recorded", body = ExamResultResponse),
        (status = 400, description = "Invalid mark, unknown user, or user not enrolled", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin), or attempted to grade yourself", body = ErrorResponse),
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
    let exam = Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &teacher) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can grade this exam",
        ));
    }

    let mark = Mark::try_new(req.mark)?;
    let target = UserId::from_key(&req.user_id);

    // Grading never targets oneself — no grader, whatever their role, may
    // write their own mark.
    if &target == teacher.get_id() {
        return Err(AppError::Forbidden("grading yourself is not allowed"));
    }

    // Target user must exist.
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };

    // ... and be enrolled in the exam's course.
    if Enrollment::read_for_user(exam.get_course(), &target, &st.db)
        .await?
        .is_none()
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user is not enrolled in this course",
        }));
    }

    let result = ExamResult::grade(&exam_id, &target, mark, teacher.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&target_user, &teacher]);
    Ok(Json(ExamResultResponse::new(&result, &people)))
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
    let people = person_map(
        results
            .iter()
            .flat_map(|r| [r.get_user().clone(), r.get_graded_by().clone()]),
        &st.db,
    )
    .await?;
    Ok(Json(
        results
            .iter()
            .map(|r| ExamResultResponse::new(r, &people))
            .collect(),
    ))
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
    let people = person_map(
        [result.get_user().clone(), result.get_graded_by().clone()],
        &st.db,
    )
    .await?;
    Ok(Json(ExamResultResponse::new(&result, &people)))
}

/// Remove a student's result from an exam. Requires teacher+ and management
/// rights over the exam's course.
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn remove_result(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can remove results",
        ));
    }
    let removed = ExamResult::remove(exam.get_id(), &UserId::from_key(&target), &st.db).await?;
    if removed.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Summary statistics for an exam's graded results. Requires teacher+.
#[utoipa::path(
    get,
    path = "/{id}/statistics",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The exam's mark statistics", body = ExamStatisticsResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_statistics(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ExamStatisticsResponse>, AppError> {
    let exam_id = ExamId::from_key(&id);
    // Exam must exist — a missing exam is a 404, not an empty statistic.
    Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let results = ExamResult::list_for_exam(&exam_id, &st.db).await?;

    let marks: Vec<i64> = results.iter().map(|r| r.get_mark().as_i64()).collect();
    let average =
        (!marks.is_empty()).then(|| marks.iter().sum::<i64>() as f64 / marks.len() as f64);
    Ok(Json(ExamStatisticsResponse {
        exam: exam_id.key().to_string(),
        graded: marks.len() as u64,
        average,
        min: marks.iter().min().copied(),
        max: marks.iter().max().copied(),
    }))
}

// ---- attempts -------------------------------------------------------------
// A scheduled exam is *sat*: starting an attempt is the live-attendance
// signal, finishing is the submission. Deadlines are judged only by the
// server clock — clients sync via `GET /time`.

/// A student's view of their attempt. `now` is echoed so clients can render
/// countdowns without trusting the device clock.
#[derive(Serialize, ToSchema)]
struct AttemptResponse {
    id: String,
    exam: String,
    user: PersonRef,
    /// When the attempt started, UTC unix-milliseconds.
    started_at: i64,
    /// Submission instant; `null` while running (or expired unsubmitted).
    finished_at: Option<i64>,
    /// `in_progress` | `submitted` | `expired`.
    #[schema(example = "in_progress")]
    status: String,
    /// When the attempt closes: `ends_at` for sync, `min(started_at +
    /// duration_ms, ends_at)` for async. Recomputed live from the exam's
    /// current schedule.
    deadline: Option<i64>,
    /// `deadline - now`, floored at 0; `null` unless in progress.
    remaining_ms: Option<i64>,
    /// The caller's mark, once graded.
    mark: Option<i64>,
    /// How many questions the caller has answered so far.
    answered: u64,
    /// How many questions the exam has.
    question_count: u64,
    /// Server clock at response time, UTC unix-milliseconds.
    now: i64,
}

impl AttemptResponse {
    fn new(
        attempt: &ExamAttempt,
        exam: &Exam,
        mark: Option<Mark>,
        people: &HashMap<String, PersonRef>,
        answered: u64,
        question_count: u64,
        now: Timestamp,
    ) -> Self {
        let status = attempt.status(exam, now);
        let deadline = attempt.deadline(exam);
        let remaining_ms = (status == AttemptStatus::InProgress)
            .then(|| deadline.map(|d| (d.as_millis() - now.as_millis()).max(0)))
            .flatten();
        Self {
            id: attempt.get_id().key().to_string(),
            exam: attempt.get_exam().key().to_string(),
            user: PersonRef::resolve(people, attempt.get_user()),
            started_at: attempt.get_started_at().as_millis(),
            finished_at: attempt.get_finished_at().map(|t| t.as_millis()),
            status: status.as_str().to_string(),
            deadline: deadline.map(|t| t.as_millis()),
            remaining_ms,
            mark: mark.map(|m| m.as_i64()),
            answered,
            question_count,
            now: now.as_millis(),
        }
    }
}

/// The (answered, question_count) pair behind an attempt view — how far one
/// student has come through the exam's question list.
async fn attempt_progress(
    exam: &ExamId,
    user: &UserId,
    db: &Database,
) -> Result<(u64, u64), AppError> {
    let answered = ExamAnswer::list_for_exam_user(exam, user, db).await?.len() as u64;
    let question_count = ExamQuestion::list_for_exam(exam, db).await?.len() as u64;
    Ok((answered, question_count))
}

/// The exam's window, or a 409 when it isn't scheduled at all. Enrollment and
/// window checks for the caller are the caller's own state — hence `Conflict`
/// (a state problem), not validation.
fn window_of(exam: &Exam) -> Result<(Timestamp, Timestamp), AppError> {
    match (exam.get_starts_at(), exam.get_ends_at()) {
        (Some(starts), Some(ends)) => Ok((starts, ends)),
        _ => Err(AppError::Conflict(
            "this exam is not scheduled — there is nothing to sit",
        )),
    }
}

/// Start (or resume) the caller's attempt at a scheduled exam. Requires
/// enrollment in the exam's course and the window to be open. Idempotent in
/// the useful direction: if an attempt already exists it is returned as-is
/// (`200` instead of `201`), so a reconnecting client gets its original clock
/// back — re-starting never resets the time.
#[utoipa::path(
    post,
    path = "/{id}/attempt",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 201, description = "Attempt started", body = AttemptResponse),
        (status = 200, description = "Attempt already existed (unchanged)", body = AttemptResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
        (status = 409, description = "Unscheduled exam, or outside the window", body = ErrorResponse),
    ),
)]
async fn start_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<AttemptResponse>), AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let (starts_at, ends_at) = window_of(&exam)?;
    if Enrollment::read_for_user(exam.get_course(), user.get_id(), &st.db)
        .await?
        .is_none()
    {
        return Err(AppError::Forbidden(
            "you are not enrolled in this exam's course",
        ));
    }
    let now = Timestamp::now();
    if now < starts_at {
        return Err(AppError::Conflict("the exam has not started yet"));
    }
    if now >= ends_at {
        return Err(AppError::Conflict("the exam has already ended"));
    }

    let (attempt, created) = ExamAttempt::start(exam.get_id(), user.get_id(), &st.db).await?;
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) = attempt_progress(exam.get_id(), user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&user]);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(AttemptResponse::new(
            &attempt,
            &exam,
            mark,
            &people,
            answered,
            question_count,
            Timestamp::now(),
        )),
    ))
}

/// The caller's own attempt: status, deadline, remaining time, and mark once
/// graded — everything a student's live exam screen needs, judged by the
/// server clock. `404` until the attempt is started.
#[utoipa::path(
    get,
    path = "/{id}/attempt",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The caller's attempt", body = AttemptResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such exam, or no attempt yet", body = ErrorResponse),
    ),
)]
async fn my_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<AttemptResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let attempt = ExamAttempt::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) = attempt_progress(exam.get_id(), user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&user]);
    Ok(Json(AttemptResponse::new(
        &attempt,
        &exam,
        mark,
        &people,
        answered,
        question_count,
        Timestamp::now(),
    )))
}

/// Submit the caller's attempt. Allowed while the deadline hasn't passed;
/// after it, the attempt is already `expired` (a valid terminal state — the
/// student used their full time) and submitting is a `409`.
#[utoipa::path(
    post,
    path = "/{id}/attempt/finish",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "Attempt submitted", body = AttemptResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such exam, or no attempt to finish", body = ErrorResponse),
        (status = 409, description = "Already submitted, or the deadline passed", body = ErrorResponse),
    ),
)]
async fn finish_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<AttemptResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let attempt = ExamAttempt::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if attempt.get_finished_at().is_some() {
        return Err(AppError::Conflict("the attempt is already submitted"));
    }
    let now = Timestamp::now();
    if let Some(deadline) = attempt.deadline(&exam)
        && now >= deadline
    {
        return Err(AppError::Conflict("time is up — the attempt has expired"));
    }

    let finished = attempt.finish(&st.db).await?;
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) = attempt_progress(exam.get_id(), user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&user]);
    Ok(Json(AttemptResponse::new(
        &finished,
        &exam,
        mark,
        &people,
        answered,
        question_count,
        Timestamp::now(),
    )))
}

// ---- live monitor ---------------------------------------------------------

/// One roster row of the live exam monitor.
#[derive(Serialize, ToSchema)]
struct LiveStudentResponse {
    user: PersonRef,
    /// `not_started` | `in_progress` | `submitted` | `expired`.
    #[schema(example = "in_progress")]
    status: String,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    /// When this student's attempt closes (server-authoritative).
    deadline: Option<i64>,
    /// Time this student has left, floored at 0; `null` unless in progress.
    remaining_ms: Option<i64>,
    /// The student's mark, once graded.
    mark: Option<i64>,
    /// How many questions this student has answered so far.
    answered: u64,
    /// When this student last saved an answer, UTC unix-milliseconds; `null`
    /// before the first save.
    last_activity: Option<i64>,
}

#[derive(Serialize, ToSchema)]
struct LiveCountsResponse {
    enrolled: u64,
    not_started: u64,
    in_progress: u64,
    submitted: u64,
    expired: u64,
    graded: u64,
}

/// A live snapshot of an exam: the enrolled roster with per-student attempt
/// state, remaining time, and marks, all judged by one server-clock read.
#[derive(Serialize, ToSchema)]
struct ExamLiveResponse {
    exam: ExamResponse,
    /// Server clock the snapshot was judged at, UTC unix-milliseconds.
    now: i64,
    /// How many questions the exam has.
    question_count: u64,
    counts: LiveCountsResponse,
    students: Vec<LiveStudentResponse>,
}

/// Build the monitor snapshot: roster ⋈ attempts ⋈ marks at one instant.
/// Students unenrolled mid-exam drop off the roster view (their attempt and
/// mark rows survive, exactly like the marks report).
async fn live_snapshot(exam: &Exam, db: &Database) -> Result<ExamLiveResponse, AppError> {
    let now = Timestamp::now();
    let roster = Enrollment::list_for_course(exam.get_course(), db).await?;
    let attempts: HashMap<String, ExamAttempt> = ExamAttempt::list_for_exam(exam.get_id(), db)
        .await?
        .into_iter()
        .map(|attempt| (attempt.get_user().key().to_string(), attempt))
        .collect();
    let marks: HashMap<String, i64> = ExamResult::list_for_exam(exam.get_id(), db)
        .await?
        .iter()
        .map(|result| {
            (
                result.get_user().key().to_string(),
                result.get_mark().as_i64(),
            )
        })
        .collect();
    let question_count = ExamQuestion::list_for_exam(exam.get_id(), db).await?.len() as u64;
    // Per-student progress: answer count and the latest save instant.
    let mut progress: HashMap<String, (u64, i64)> = HashMap::new();
    for answer in ExamAnswer::list_for_exam(exam.get_id(), db).await? {
        let entry = progress
            .entry(answer.get_user().key().to_string())
            .or_insert((0, i64::MIN));
        entry.0 += 1;
        entry.1 = entry.1.max(answer.get_updated_at().as_millis());
    }
    let people = person_map(roster.iter().map(|e| e.get_user().clone()), db).await?;

    let mut students: Vec<LiveStudentResponse> = roster
        .iter()
        .map(|enrollment| {
            let key = enrollment.get_user().key();
            let attempt = attempts.get(key);
            let status = attempt.map(|a| a.status(exam, now));
            let deadline = attempt.and_then(|a| a.deadline(exam));
            LiveStudentResponse {
                user: PersonRef::resolve(&people, enrollment.get_user()),
                status: status
                    .map_or("not_started", AttemptStatus::as_str)
                    .to_string(),
                started_at: attempt.map(|a| a.get_started_at().as_millis()),
                finished_at: attempt
                    .and_then(|a| a.get_finished_at())
                    .map(|t| t.as_millis()),
                deadline: deadline.map(|t| t.as_millis()),
                remaining_ms: (status == Some(AttemptStatus::InProgress))
                    .then(|| deadline.map(|d| (d.as_millis() - now.as_millis()).max(0)))
                    .flatten(),
                mark: marks.get(key).copied(),
                answered: progress.get(key).map_or(0, |p| p.0),
                last_activity: progress.get(key).map(|p| p.1),
            }
        })
        .collect();
    students.sort_by(|a, b| a.user.username.cmp(&b.user.username));

    let count = |status: &str| students.iter().filter(|s| s.status == status).count() as u64;
    let counts = LiveCountsResponse {
        enrolled: students.len() as u64,
        not_started: count("not_started"),
        in_progress: count("in_progress"),
        submitted: count("submitted"),
        expired: count("expired"),
        graded: students.iter().filter(|s| s.mark.is_some()).count() as u64,
    };
    Ok(ExamLiveResponse {
        exam: ExamResponse::new(exam),
        now: now.as_millis(),
        question_count,
        counts,
        students,
    })
}

/// A one-shot live snapshot of the exam: who's in, who's still writing, time
/// each student has left, and marks as they land. Requires teacher+. For a
/// self-updating feed of the same shape, see `GET /exams/{id}/live/stream`.
#[utoipa::path(
    get,
    path = "/{id}/live",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "Live snapshot", body = ExamLiveResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_live(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ExamLiveResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(live_snapshot(&exam, &st.db).await?))
}

/// The live snapshot as a Server-Sent-Events stream: one `snapshot` event
/// (the `ExamLiveResponse` JSON) immediately on connect and then every couple
/// of seconds, so attendance, remaining time, submissions, and marks update
/// without polling. Requires teacher+. Consume with `EventSource` (cookies
/// ride along on same-site / credentialed requests). If the exam disappears
/// mid-stream an `error` event is sent instead.
#[utoipa::path(
    get,
    path = "/{id}/live/stream",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "SSE feed of `snapshot` events (`ExamLiveResponse` as JSON)", content_type = "text/event-stream"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_live_stream(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::Error>>>, AppError> {
    let exam_id = ExamId::from_key(&id);
    // A missing exam is a 404 up front; after this the response is a stream.
    Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    // First tick fires immediately, so the monitor paints on connect. The
    // exam is re-read every tick: schedule edits (deadline extensions) show
    // up mid-stream.
    let interval = tokio::time::interval(Duration::from_secs(EXAM_LIVE_STREAM_INTERVAL_SECS));
    let stream = IntervalStream::new(interval).then(move |_| {
        let db = st.db.clone();
        let exam_id = exam_id.clone();
        async move {
            let snapshot = match Exam::read(&exam_id, &db).await {
                Ok(Some(exam)) => live_snapshot(&exam, &db).await,
                Ok(None) => Err(AppError::NotFound),
                Err(err) => Err(err),
            };
            match snapshot {
                Ok(snapshot) => Event::default().event("snapshot").json_data(&snapshot),
                // Exam deleted mid-stream or a db hiccup: say so without
                // leaking internals and keep the stream alive — the client
                // decides whether to hang on or close.
                Err(err) => {
                    tracing::warn!("live exam stream snapshot failed: {err}");
                    Event::default()
                        .event("error")
                        .json_data(serde_json::json!({ "error": "live snapshot unavailable" }))
                }
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---- questions --------------------------------------------------------------
// Teachers author the question list before the exam runs; it freezes the
// moment anyone starts an attempt, so every student sits the same exam.

#[derive(Deserialize, ToSchema)]
struct CreateQuestion {
    /// The question itself.
    #[schema(example = "What is 2 + 2?")]
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    /// This question's share of the auto-score, `1`–`100`.
    #[schema(example = 10)]
    points: i64,
    /// The options of a `choice` question (2–10 of them); omit for `text`.
    choices: Option<Vec<String>>,
    /// Zero-based index of the right option; required for `choice`, absent
    /// for `text`.
    #[schema(example = 1)]
    correct: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateQuestion {
    text: Option<String>,
    points: Option<i64>,
    /// `choice` or `text`. Switching kinds needs the other fields to follow:
    /// send `choices` + `correct` when moving to `choice`, explicit `null`s
    /// when moving to `text`.
    kind: Option<String>,
    /// Omit to keep the stored options; send `null` to drop them (text
    /// questions only).
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<Vec<String>>)]
    choices: Option<Option<Vec<String>>>,
    /// Omit to keep; `null` to clear (text questions only).
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    correct: Option<Option<i64>>,
}

/// A question as its author sees it — including the `correct` index. Never
/// serialized to students; they get [`AttemptQuestionResponse`].
#[derive(Serialize, ToSchema)]
struct QuestionResponse {
    id: String,
    exam: String,
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    points: i64,
    choices: Option<Vec<String>>,
    /// Zero-based index of the right option (`choice` questions only).
    correct: Option<i64>,
}

impl QuestionResponse {
    fn new(question: &ExamQuestion) -> Self {
        Self {
            id: question.get_id().key().to_string(),
            exam: question.get_exam().key().to_string(),
            text: question.get_text().as_str().to_string(),
            kind: question.get_kind().as_str().to_string(),
            points: question.get_points().as_i64(),
            choices: question
                .get_choices()
                .map(|choices| choices.iter().map(|c| c.as_str().to_string()).collect()),
            correct: question.get_correct(),
        }
    }
}

/// The questions freeze once anyone has started an attempt — editing them
/// under a student mid-exam would fork what "the exam" means.
async fn ensure_questions_editable(exam: &ExamId, db: &Database) -> Result<(), AppError> {
    if ExamAttempt::any_for_exam(exam, db).await? {
        return Err(AppError::Conflict(
            "cannot change questions after attempts have started",
        ));
    }
    Ok(())
}

/// The question, provided it belongs to `exam` — a qid under someone else's
/// exam is a plain 404, not a leak.
async fn question_of_exam(
    exam: &ExamId,
    qid: &str,
    db: &Database,
) -> Result<ExamQuestion, AppError> {
    let question = ExamQuestion::read(&ExamQuestionId::from_key(qid), db)
        .await?
        .ok_or(AppError::NotFound)?;
    if question.get_exam() != exam {
        return Err(AppError::NotFound);
    }
    Ok(question)
}

/// Add a question to an exam. Requires teacher+ and management rights over the
/// exam's course. `choice` questions carry 2–10 `choices` plus the `correct`
/// index; `text` questions carry neither. Locked once attempts exist.
#[utoipa::path(
    post,
    path = "/{id}/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = CreateQuestion,
    responses(
        (status = 201, description = "Question created", body = QuestionResponse),
        (status = 400, description = "Invalid text, kind, points, choices, or correct", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn create_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateQuestion>,
) -> Result<(StatusCode, Json<QuestionResponse>), AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can author questions",
        ));
    }
    ensure_questions_editable(exam.get_id(), &st.db).await?;

    let text = QuestionText::try_new(&req.text)?;
    let points = QuestionPoints::try_new(req.points)?;
    let spec = QuestionSpec::try_new(QuestionKind::try_new(&req.kind)?, req.choices, req.correct)?;
    let question = ExamQuestion::create(exam.get_id(), text, points, spec, &st.db).await?;
    Ok((StatusCode::CREATED, Json(QuestionResponse::new(&question))))
}

/// The exam's full question list, `correct` indexes included. Requires
/// teacher+. Students read questions through `GET /exams/{id}/attempt/questions`.
#[utoipa::path(
    get,
    path = "/{id}/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The exam's questions", body = [QuestionResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn list_questions(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<Vec<QuestionResponse>>, AppError> {
    let exam_id = ExamId::from_key(&id);
    Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let questions = ExamQuestion::list_for_exam(&exam_id, &st.db).await?;
    Ok(Json(questions.iter().map(QuestionResponse::new).collect()))
}

/// Edit a question. Requires teacher+ and management rights over the exam's
/// course. Omitted fields keep their value; `kind`/`choices`/`correct` are
/// re-validated as a unit, so a kind switch must bring the matching fields
/// along. Locked once attempts exist.
#[utoipa::path(
    patch,
    path = "/{id}/questions/{qid}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    request_body = UpdateQuestion,
    responses(
        (status = 200, description = "Updated question", body = QuestionResponse),
        (status = 400, description = "Invalid text, kind, points, choices, or correct", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no such question in it", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn update_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
    Json(req): Json<UpdateQuestion>,
) -> Result<Json<QuestionResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can edit questions",
        ));
    }
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;

    let text = match req.text {
        Some(ref text) => QuestionText::try_new(text)?,
        None => question.get_text().clone(),
    };
    let points = match req.points {
        Some(points) => QuestionPoints::try_new(points)?,
        None => question.get_points(),
    };
    // Merge the kind-dependent fields (set / clear / keep per field), then
    // re-validate them as a unit — a PATCH can't leave a half-question behind.
    let kind = match req.kind {
        Some(ref kind) => QuestionKind::try_new(kind)?,
        None => question.get_kind().clone(),
    };
    let choices = match req.choices {
        Some(update) => update,
        None => question
            .get_choices()
            .map(|choices| choices.iter().map(|c| c.as_str().to_string()).collect()),
    };
    let correct = match req.correct {
        Some(update) => update,
        None => question.get_correct(),
    };
    let spec = QuestionSpec::try_new(kind, choices, correct)?;

    let updated = question.update(text, points, spec, &st.db).await?;
    Ok(Json(QuestionResponse::new(&updated)))
}

/// Remove a question (and every answer to it). Requires teacher+ and
/// management rights over the exam's course. Locked once attempts exist.
#[utoipa::path(
    delete,
    path = "/{id}/questions/{qid}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no such question in it", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn delete_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can delete questions",
        ));
    }
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    question.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- answers ----------------------------------------------------------------
// Students answer inside their attempt; every save is an upsert stamped with
// the server clock. The WebSocket room (`/exams/{id}/attempt/ws`) drives the
// same paths below.

/// A student's own saved answer, embedded in their question view.
#[derive(Serialize, ToSchema)]
struct AnswerStateResponse {
    /// The picked option's zero-based index (`choice` questions).
    selected: Option<i64>,
    /// The typed answer (`text` questions).
    text: Option<String>,
    /// When this answer was last saved, UTC unix-milliseconds.
    updated_at: i64,
}

impl AnswerStateResponse {
    fn new(answer: &ExamAnswer) -> Self {
        Self {
            selected: answer.get_selected(),
            text: answer.get_text().map(|t| t.as_str().to_string()),
            updated_at: answer.get_updated_at().as_millis(),
        }
    }
}

/// A question as the sitting student sees it: no `correct` index, their own
/// saved answer embedded.
#[derive(Serialize, ToSchema)]
struct AttemptQuestionResponse {
    id: String,
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    points: i64,
    choices: Option<Vec<String>>,
    /// The caller's saved answer; `null` while unanswered.
    answer: Option<AnswerStateResponse>,
}

#[derive(Deserialize, ToSchema)]
struct SaveAnswer {
    /// The question being answered.
    question_id: String,
    /// The picked option's zero-based index — required for `choice` questions.
    selected: Option<i64>,
    /// The typed answer — required for `text` questions (empty clears the draft).
    text: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct AnswerSavedResponse {
    question: String,
    selected: Option<i64>,
    text: Option<String>,
    /// Save instant by the server clock, UTC unix-milliseconds.
    updated_at: i64,
}

/// The caller's attempt provided it is still writable, or the error that says
/// why not: no attempt yet (404 — start it first), already submitted (409),
/// deadline passed (409). One gate shared by REST saves and the WebSocket room.
pub(crate) async fn writable_attempt(
    exam: &Exam,
    user: &UserId,
    db: &Database,
) -> Result<ExamAttempt, AppError> {
    let attempt = ExamAttempt::read_for_user(exam.get_id(), user, db)
        .await?
        .ok_or(AppError::NotFound)?;
    match attempt.status(exam, Timestamp::now()) {
        AttemptStatus::Submitted => Err(AppError::Conflict("the attempt is already submitted")),
        AttemptStatus::Expired => Err(AppError::Conflict("time is up — the attempt has expired")),
        AttemptStatus::InProgress => Ok(attempt),
    }
}

/// Save one answer inside the caller's in-progress attempt — the whole write
/// path (attempt gate, question lookup, kind check, upsert), shared verbatim
/// by the REST handler and the WebSocket room so the two can never drift.
pub(crate) async fn save_answer_checked(
    exam: &Exam,
    user: &UserId,
    question_id: &str,
    selected: Option<i64>,
    text: Option<String>,
    db: &Database,
) -> Result<ExamAnswer, AppError> {
    writable_attempt(exam, user, db).await?;
    let question = question_of_exam(exam.get_id(), question_id, db).await?;
    ExamAnswer::save(&question, user, selected, text, db).await
}

/// The exam's questions as the sitting student sees them: `correct` stripped,
/// their own saved answers embedded. Requires an attempt — start one with
/// `POST /exams/{id}/attempt` first (404 until then). Readable in every
/// attempt state, so a submitted student can still review what they wrote.
#[utoipa::path(
    get,
    path = "/{id}/attempt/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "Questions with the caller's answers embedded", body = [AttemptQuestionResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such exam, or no attempt yet — start the attempt first", body = ErrorResponse),
    ),
)]
async fn attempt_questions(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<AttemptQuestionResponse>>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // The question list is for sitting students; without an attempt there is
    // nothing to sit behind — and no early peek at the questions.
    ExamAttempt::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let questions = ExamQuestion::list_for_exam(exam.get_id(), &st.db).await?;
    let answers: HashMap<String, ExamAnswer> =
        ExamAnswer::list_for_exam_user(exam.get_id(), user.get_id(), &st.db)
            .await?
            .into_iter()
            .map(|answer| (answer.get_question().key().to_string(), answer))
            .collect();
    Ok(Json(
        questions
            .iter()
            .map(|question| AttemptQuestionResponse {
                id: question.get_id().key().to_string(),
                text: question.get_text().as_str().to_string(),
                kind: question.get_kind().as_str().to_string(),
                points: question.get_points().as_i64(),
                choices: question
                    .get_choices()
                    .map(|choices| choices.iter().map(|c| c.as_str().to_string()).collect()),
                answer: answers
                    .get(question.get_id().key())
                    .map(AnswerStateResponse::new),
            })
            .collect(),
    ))
}

/// Save (or overwrite) one answer in the caller's in-progress attempt.
/// `choice` questions take `selected`; `text` questions take `text`. Rejected
/// once the attempt is submitted or its deadline has passed — the server
/// clock, not the client's, is the judge.
#[utoipa::path(
    post,
    path = "/{id}/attempt/answers",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = SaveAnswer,
    responses(
        (status = 200, description = "Answer saved", body = AnswerSavedResponse),
        (status = 400, description = "Payload doesn't match the question's kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or attempt", body = ErrorResponse),
        (status = 409, description = "Attempt already submitted, or time is up", body = ErrorResponse),
    ),
)]
async fn save_answer(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SaveAnswer>,
) -> Result<Json<AnswerSavedResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let answer = save_answer_checked(
        &exam,
        user.get_id(),
        &req.question_id,
        req.selected,
        req.text,
        &st.db,
    )
    .await?;
    Ok(Json(AnswerSavedResponse {
        question: answer.get_question().key().to_string(),
        selected: answer.get_selected(),
        text: answer.get_text().map(|t| t.as_str().to_string()),
        updated_at: answer.get_updated_at().as_millis(),
    }))
}

/// One student's answer sheet with correctness flags. Every row carries the
/// saved answer plus `is_correct` (`null` for text questions — those are the
/// grader's call), and the machine's `auto_score` over the choice questions is
/// attached as a *suggestion*: the final mark stays human, via
/// `POST /exams/{id}/results`. Requires teacher+.
#[utoipa::path(
    get,
    path = "/{id}/attempts/{user}/answers",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 200, description = "The student's answers, judged", body = AttemptAnswersResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such exam, or the student has no attempt", body = ErrorResponse),
    ),
)]
async fn attempt_answers(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let target = UserId::from_key(&target);
    // No attempt means no answer sheet — a 404, not an empty one.
    ExamAttempt::read_for_user(exam.get_id(), &target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let questions = ExamQuestion::list_for_exam(exam.get_id(), &st.db).await?;
    let answers = ExamAnswer::list_for_exam_user(exam.get_id(), &target, &st.db).await?;
    let by_question: HashMap<&str, &ExamQuestion> = questions
        .iter()
        .map(|question| (question.get_id().key(), question))
        .collect();
    let (earned, possible) = auto_score(&questions, &answers);
    let people = person_map([target.clone()], &st.db).await?;
    Ok(Json(AttemptAnswersResponse {
        exam: exam.get_id().key().to_string(),
        user: PersonRef::resolve(&people, &target),
        answers: answers
            .iter()
            .map(|answer| StudentAnswerResponse {
                question: answer.get_question().key().to_string(),
                selected: answer.get_selected(),
                text: answer.get_text().map(|t| t.as_str().to_string()),
                updated_at: answer.get_updated_at().as_millis(),
                is_correct: by_question
                    .get(answer.get_question().key())
                    .and_then(|question| answer.is_correct(question)),
            })
            .collect(),
        auto_score: AutoScoreResponse { earned, possible },
    }))
}

/// One row of a student's answer sheet, as the grader sees it.
#[derive(Serialize, ToSchema)]
struct StudentAnswerResponse {
    question: String,
    selected: Option<i64>,
    text: Option<String>,
    /// When the answer was last saved, UTC unix-milliseconds.
    updated_at: i64,
    /// Whether `selected` hits the question's `correct`; `null` for text
    /// questions (the grader judges those).
    is_correct: Option<bool>,
}

/// The machine's scoring suggestion over the choice questions.
#[derive(Serialize, ToSchema)]
struct AutoScoreResponse {
    /// Points earned where `selected == correct`.
    earned: i64,
    /// Total points across the exam's choice questions.
    possible: i64,
}

/// A student's full answer sheet for grading.
#[derive(Serialize, ToSchema)]
struct AttemptAnswersResponse {
    exam: String,
    /// The student whose sheet this is.
    user: PersonRef,
    answers: Vec<StudentAnswerResponse>,
    /// The suggested score over choice questions — never the final mark.
    auto_score: AutoScoreResponse,
}
