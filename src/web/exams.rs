use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
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
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode,
    ExamSchedule, ExamTitle,
};
use crate::domain::exam_answer::{ExamAnswer, auto_score};
use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt};
use crate::domain::exam_question::{
    ExamQuestion, ExamQuestionId, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::exam_result::{ExamResult, Mark};
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course, visible_courses};
use super::{
    CurrentUser, ExamResponse, Page, PageParams, PersonRef, RequireTeacher, check_not_past,
    paginate, person_map, set_or_clear,
};

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
    /// The assessment form — one of the school's exam kinds (`GET /settings`).
    /// Changing it re-weights the exam: the course average uses the kind's
    /// settings-configured weight.
    kind: Option<String>,
    /// `sync`, `async`, or `open`. Omit to keep the current mode; send `null`
    /// to turn the exam back into an offline draft. Frozen once anyone has
    /// started an attempt. Switching to `open` requires clearing
    /// `starts_at`/`ends_at` in the same request.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    mode: Option<Option<String>>,
    /// Window open, UTC unix-milliseconds. Omit to keep; `null` to clear.
    /// A newly set value must not be in the past.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    starts_at: Option<Option<i64>>,
    /// Window close, UTC unix-milliseconds. Omit to keep; `null` to clear.
    /// Moving it while a sync exam runs extends (or cuts) everyone's deadline;
    /// a newly set value must not be in the past.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    ends_at: Option<Option<i64>>,
    /// Per-attempt budget, milliseconds (`async`, or optionally `open`). Omit
    /// to keep; `null` to clear. Changing it mid-exam moves every running
    /// attempt's deadline.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    duration_ms: Option<Option<i64>>,
    /// Attempt limit: `1`–`100`, or `0` for unlimited. Omit to keep. Editable
    /// live — raising it grants retakes on the spot; lowering it only blocks
    /// future starts.
    max_attempts: Option<i64>,
    /// Whether students who left the exam room may come back in. Omit to
    /// keep. Editable live — the teacher's door handle for the running room.
    allow_rejoin: Option<bool>,
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

/// List the exams visible to the caller: every exam for manager+, otherwise
/// the exams of the courses they created or are enrolled in. Paged via
/// `?limit=&offset=` (omit `limit` for the full list); returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "exams",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's visible exams (all of them when unpaged)", body = Page<ExamResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_exams(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ExamResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let exams = if user.get_role().at_least(Role::Manager) {
        Exam::list_all(&st.db).await?
    } else {
        let courses = visible_courses(&user, &st.db).await?;
        let ids: Vec<_> = courses.iter().map(|c| c.get_id().clone()).collect();
        Exam::list_for_courses(&ids, &st.db).await?
    };
    let total = exams.len() as i64;
    let items = paginate(&exams, limit, offset)
        .iter()
        .map(ExamResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single exam by id. Visible to its course's enrolled users, the
/// course creator, and managers/admins.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The exam", body = ExamResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course, not its creator, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_exam(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<ExamResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, or a manager/admin can view this exam",
        ));
    }
    Ok(Json(ExamResponse::new(&exam)))
}

/// Update an exam. Requires teacher+ and management rights over the exam's
/// course (its creator, or manager/admin). Omitted fields keep their value; an
/// explicit `null` clears a schedule field; the course itself is not updatable.
/// The schedule must stay consistent as a whole (see the create endpoint), and
/// `mode` is frozen once anyone has started an attempt — times, duration,
/// `max_attempts`, and `allow_rejoin` stay editable so a running exam can be
/// extended, granted retakes, or have its rejoin door opened live.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = UpdateExam,
    responses(
        (status = 200, description = "Updated exam", body = ExamResponse),
        (status = 400, description = "Invalid fields, kind, attempt limit, or schedule (malformed window, or newly set times in the past)", body = ErrorResponse),
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
        // Only a kind this request sets is held to the current settings list —
        // a stored kind survives later list edits, like past times survive
        // the no-past rule.
        Some(ref kind) => {
            let school = Settings::load(&st.db).await?;
            ExamKind::try_new(kind, school.get_exam_kinds())?
        }
        None => exam.get_kind().clone(),
    };

    // Merge the schedule (set / clear / keep per field), then re-validate it
    // as a unit — a PATCH can't leave a half-schedule behind. Only values this
    // request sets are held to the no-past rule: kept ones may legitimately be
    // past (a running exam's `starts_at`), and rechecking them would block
    // unrelated edits.
    let mode = match req.mode {
        Some(update) => update.as_deref().map(ExamMode::try_new).transpose()?,
        None => exam.get_mode().cloned(),
    };
    let starts_at = match req.starts_at {
        Some(update) => {
            let starts_at = update.map(Timestamp::from_millis);
            check_not_past("starts_at", starts_at)?;
            starts_at
        }
        None => exam.get_starts_at(),
    };
    let ends_at = match req.ends_at {
        Some(update) => {
            let ends_at = update.map(Timestamp::from_millis);
            check_not_past("ends_at", ends_at)?;
            ends_at
        }
        None => exam.get_ends_at(),
    };
    let duration_ms = match req.duration_ms {
        Some(update) => update.map(ExamDuration::try_new).transpose()?,
        None => exam.get_duration_ms(),
    };
    let schedule = ExamSchedule::try_new(mode, starts_at, ends_at, duration_ms)?;
    let max_attempts = match req.max_attempts {
        Some(limit) => ExamAttemptLimit::try_new(limit)?,
        None => exam.get_max_attempts(),
    };
    let allow_rejoin = req.allow_rejoin.unwrap_or_else(|| exam.get_allow_rejoin());

    // Switching sync <-> async <-> open (or back to a draft) would silently
    // rewrite the deadline rules under students who already sat down;
    // extending times, the attempt limit, and the rejoin door are the
    // supported live adjustments instead.
    let mode_changed =
        schedule.get_mode().map(ExamMode::as_str) != exam.get_mode().map(ExamMode::as_str);
    if mode_changed && ExamAttempt::any_for_exam(exam.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "cannot change the exam mode after attempts have started",
        ));
    }

    let updated = exam
        .update(
            title,
            description,
            kind,
            schedule,
            max_attempts,
            allow_rejoin,
            &st.db,
        )
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

/// List an exam's results, paged via `?limit=&offset=` (omit `limit` for all
/// of them). Requires teacher+ and management rights over the exam's course —
/// students read only their own via `GET /exams/{id}/result`. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/results",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id"), PageParams),
    responses(
        (status = 200, description = "A page of results (all of them when unpaged)", body = Page<ExamResultResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn list_results(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ExamResultResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Exam must exist — a missing exam is a 404, not an empty result list.
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can list results",
        ));
    }
    let results = ExamResult::list_for_exam(exam.get_id(), &st.db).await?;
    let total = results.len() as i64;
    // Join people onto the page alone — the lookup shrinks with the window.
    let rows = paginate(&results, limit, offset);
    let people = person_map(
        rows.iter()
            .flat_map(|r| [r.get_user().clone(), r.get_graded_by().clone()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|r| ExamResultResponse::new(r, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
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

/// Summary statistics for an exam's graded results. Requires teacher+ and
/// management rights over the exam's course.
#[utoipa::path(
    get,
    path = "/{id}/statistics",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The exam's mark statistics", body = ExamStatisticsResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_statistics(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ExamStatisticsResponse>, AppError> {
    // Exam must exist — a missing exam is a 404, not an empty statistic.
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can view statistics",
        ));
    }
    let results = ExamResult::list_for_exam(exam.get_id(), &st.db).await?;

    let marks: Vec<i64> = results.iter().map(|r| r.get_mark().as_i64()).collect();
    let average =
        (!marks.is_empty()).then(|| marks.iter().sum::<i64>() as f64 / marks.len() as f64);
    Ok(Json(ExamStatisticsResponse {
        exam: exam.get_id().key().to_string(),
        graded: marks.len() as u64,
        average,
        min: marks.iter().min().copied(),
        max: marks.iter().max().copied(),
    }))
}

// ---- attempts -------------------------------------------------------------
// A sittable exam (sync, async, or open mode) is *sat*: starting an attempt
// is the live-attendance signal, finishing is the submission. The exam's
// `max_attempts` (0 = unlimited) says how many sittings each student gets;
// re-posting resumes a running attempt and mints the next sitting once the
// last one is over. Deadlines are judged only by the server clock — clients
// sync via `GET /time`.

/// A student's view of their (latest) attempt. `now` is echoed so clients can
/// render countdowns without trusting the device clock.
#[derive(Serialize, ToSchema)]
struct AttemptResponse {
    id: String,
    exam: String,
    user: PersonRef,
    /// Which sitting this is — 1 for the first attempt, counting up.
    attempt: i64,
    /// How many sittings the caller has used, this one included.
    attempts_used: u64,
    /// The exam's attempt limit; `0` means unlimited.
    max_attempts: i64,
    /// When the attempt started, UTC unix-milliseconds.
    started_at: i64,
    /// Submission instant; `null` while running (or expired unsubmitted).
    finished_at: Option<i64>,
    /// When the student left the exam room mid-attempt; `null` while inside
    /// (or if they never used the room). With `allow_rejoin` off, a set
    /// `left_at` locks further answering until the teacher reopens the door.
    left_at: Option<i64>,
    /// `in_progress` | `submitted` | `expired`.
    #[schema(example = "in_progress")]
    status: String,
    /// When the attempt closes: the earlier of the window's `ends_at` and
    /// `started_at + duration_ms` (whichever exists). Recomputed live from
    /// the exam's current schedule; `null` for an open exam without a
    /// duration — such an attempt only ends by submission.
    deadline: Option<i64>,
    /// `deadline - now`, floored at 0; `null` unless in progress with a
    /// deadline.
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
    #[expect(
        clippy::too_many_arguments,
        reason = "a flat view over attempt + exam + progress; a builder would obscure it"
    )]
    fn new(
        attempt: &ExamAttempt,
        exam: &Exam,
        mark: Option<Mark>,
        people: &HashMap<String, PersonRef>,
        answered: u64,
        question_count: u64,
        attempts_used: u64,
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
            attempt: attempt.get_seq(),
            attempts_used,
            max_attempts: exam.get_max_attempts().as_i64(),
            started_at: attempt.get_started_at().as_millis(),
            finished_at: attempt.get_finished_at().map(|t| t.as_millis()),
            left_at: attempt.get_left_at().map(|t| t.as_millis()),
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

/// A 409 unless the exam can be sat at all: it needs a mode (`sync`, `async`,
/// or `open`) — a modeless exam is an offline-graded draft. Enrollment and
/// window checks for the caller are the caller's own state — hence `Conflict`
/// (a state problem), not validation.
fn ensure_sittable(exam: &Exam) -> Result<(), AppError> {
    if exam.get_mode().is_none() {
        return Err(AppError::Conflict(
            "this exam is not scheduled — there is nothing to sit (give it a mode: sync, async, or open)",
        ));
    }
    Ok(())
}

/// How many sittings the caller has used at this exam.
async fn attempts_used(exam: &ExamId, user: &UserId, db: &Database) -> Result<u64, AppError> {
    Ok(ExamAttempt::list_for_user(exam, user, db).await?.len() as u64)
}

/// Start, resume, or retake the caller's attempt. Requires enrollment in the
/// exam's course, a sittable exam (`sync`/`async`/`open` mode), and — when a
/// window exists — the window to be open. A still-running attempt is returned
/// as-is (`200` instead of `201`), so a reconnecting client gets its original
/// clock back — re-starting never resets the time. Once the latest attempt is
/// submitted or expired, re-posting starts the next sitting (`201`, blank
/// answer sheet) while the exam's `max_attempts` (0 = unlimited) allows it.
#[utoipa::path(
    post,
    path = "/{id}/attempt",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 201, description = "Attempt started (first sitting or a retake)", body = AttemptResponse),
        (status = 200, description = "Running attempt resumed (unchanged)", body = AttemptResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
        (status = 409, description = "Draft exam, outside the window, or no attempts remaining", body = ErrorResponse),
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
    ensure_sittable(&exam)?;
    ensure_enrolled(&exam, user.get_id(), &st.db).await?;
    let now = Timestamp::now();
    if let Some(starts_at) = exam.get_starts_at()
        && now < starts_at
    {
        return Err(AppError::Conflict("the exam has not started yet"));
    }
    if let Some(ends_at) = exam.get_ends_at()
        && now >= ends_at
    {
        return Err(AppError::Conflict("the exam has already ended"));
    }

    let (attempt, created) = ExamAttempt::start(&exam, user.get_id(), &st.db).await?;
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) = attempt_progress(exam.get_id(), user.get_id(), &st.db).await?;
    let used = attempts_used(exam.get_id(), user.get_id(), &st.db).await?;
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
            used,
            Timestamp::now(),
        )),
    ))
}

/// The caller's own (latest) attempt: status, deadline, remaining time, and
/// mark once graded — everything a student's live exam screen needs, judged
/// by the server clock. `404` until an attempt is started.
#[utoipa::path(
    get,
    path = "/{id}/attempt",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The caller's latest attempt", body = AttemptResponse),
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
    let attempt = ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) = attempt_progress(exam.get_id(), user.get_id(), &st.db).await?;
    let used = attempts_used(exam.get_id(), user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&user]);
    Ok(Json(AttemptResponse::new(
        &attempt,
        &exam,
        mark,
        &people,
        answered,
        question_count,
        used,
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
    let attempt = ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db)
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

    // Deliberately no rejoin check: a student locked out of the room may
    // still submit what they saved — finishing answers nothing new.
    let finished = attempt.finish(&st.db).await?;
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) = attempt_progress(exam.get_id(), user.get_id(), &st.db).await?;
    let used = attempts_used(exam.get_id(), user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&user]);
    Ok(Json(AttemptResponse::new(
        &finished,
        &exam,
        mark,
        &people,
        answered,
        question_count,
        used,
        Timestamp::now(),
    )))
}

// ---- live monitor ---------------------------------------------------------

/// One roster row of the live exam monitor. The attempt fields describe the
/// student's *latest* sitting.
#[derive(Serialize, ToSchema)]
struct LiveStudentResponse {
    user: PersonRef,
    /// `not_started` | `absent` | `in_progress` | `submitted` | `expired`.
    /// `absent` is `not_started` after the window closed: the student never
    /// sat and no longer can. Open exams have no window, so nobody is ever
    /// absent from one.
    #[schema(example = "in_progress")]
    status: String,
    /// Which sitting the shown attempt is (1, 2, …); `null` before the first.
    attempt: Option<i64>,
    /// How many sittings this student has used.
    attempts_used: u64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    /// When this student left the exam room mid-attempt; `null` while inside.
    left_at: Option<i64>,
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
    /// Enrolled with no attempt while the window is still open (or the exam
    /// has no window).
    not_started: u64,
    /// Enrolled with no attempt and the window closed — the no-shows.
    absent: u64,
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
    // Per student: their latest sitting (the one the monitor shows) plus how
    // many they've used.
    let mut attempts: HashMap<String, ExamAttempt> = HashMap::new();
    let mut used: HashMap<String, u64> = HashMap::new();
    for attempt in ExamAttempt::list_for_exam(exam.get_id(), db).await? {
        let key = attempt.get_user().key().to_string();
        *used.entry(key.clone()).or_insert(0) += 1;
        match attempts.entry(key) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(attempt);
            }
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if attempt.get_seq() > slot.get().get_seq() {
                    slot.insert(attempt);
                }
            }
        }
    }
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

    // Once the window closes the door is shut for good (starting answers
    // 409), so "hasn't started" hardens into "was absent". Open exams have
    // no window and never make that call.
    let window_over = exam.get_ends_at().is_some_and(|ends| now >= ends);
    let mut students: Vec<LiveStudentResponse> = roster
        .iter()
        .map(|enrollment| {
            let key = enrollment.get_user().key();
            let attempt = attempts.get(key);
            let status = attempt.map(|a| a.status(exam, now));
            let deadline = attempt.and_then(|a| a.deadline(exam));
            LiveStudentResponse {
                user: PersonRef::resolve(&people, enrollment.get_user()),
                status: match status {
                    Some(status) => status.as_str(),
                    None if window_over => "absent",
                    None => "not_started",
                }
                .to_string(),
                attempt: attempt.map(ExamAttempt::get_seq),
                attempts_used: used.get(key).copied().unwrap_or(0),
                started_at: attempt.map(|a| a.get_started_at().as_millis()),
                finished_at: attempt
                    .and_then(|a| a.get_finished_at())
                    .map(|t| t.as_millis()),
                left_at: attempt.and_then(|a| a.get_left_at()).map(|t| t.as_millis()),
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
        absent: count("absent"),
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

/// A one-shot live snapshot of the exam: who's in, who's still writing (and
/// on which sitting), who walked out of the room (`left_at`), who never
/// showed at all (`absent`, once the window is over), time each student has
/// left, and marks as they land. Requires teacher+ and management rights
/// over the exam's course. For a self-updating feed of the same shape, see
/// `GET /exams/{id}/live/stream`.
#[utoipa::path(
    get,
    path = "/{id}/live",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "Live snapshot", body = ExamLiveResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_live(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ExamLiveResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can monitor this exam",
        ));
    }
    Ok(Json(live_snapshot(&exam, &st.db).await?))
}

/// The live snapshot as a Server-Sent-Events stream: one `snapshot` event
/// (the `ExamLiveResponse` JSON) immediately on connect and then every couple
/// of seconds, so attendance, remaining time, submissions, and marks update
/// without polling. Requires teacher+ and management rights over the exam's
/// course. Consume with `EventSource` (cookies ride along on same-site /
/// credentialed requests). If the exam disappears mid-stream an `error` event
/// is sent instead.
#[utoipa::path(
    get,
    path = "/{id}/live/stream",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "SSE feed of `snapshot` events (`ExamLiveResponse` as JSON)", content_type = "text/event-stream"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_live_stream(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::Error>>>, AppError> {
    let exam_id = ExamId::from_key(&id);
    // A missing exam is a 404 up front; after this the response is a stream.
    let exam = Exam::read(&exam_id, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can monitor this exam",
        ));
    }

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

/// The exam's question list, `correct` indexes included — the answer key,
/// paged via `?limit=&offset=` (omit `limit` for the whole list). Requires
/// teacher+ and management rights over the exam's course. Students read
/// questions through `GET /exams/{id}/attempt/questions`. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id"), PageParams),
    responses(
        (status = 200, description = "A page of the exam's questions (all of them when unpaged)", body = Page<QuestionResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn list_questions(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<QuestionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can read the question list",
        ));
    }
    let questions = ExamQuestion::list_for_exam(exam.get_id(), &st.db).await?;
    let total = questions.len() as i64;
    let items = paginate(&questions, limit, offset)
        .iter()
        .map(QuestionResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
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

/// The caller's latest attempt provided it is still writable, or the error
/// that says why not: no attempt yet (404 — start it first), already
/// submitted (409), deadline passed (409). One gate shared by REST saves and
/// the WebSocket room.
pub(crate) async fn writable_attempt(
    exam: &Exam,
    user: &UserId,
    db: &Database,
) -> Result<ExamAttempt, AppError> {
    let attempt = ExamAttempt::read_latest_for_user(exam.get_id(), user, db)
        .await?
        .ok_or(AppError::NotFound)?;
    match attempt.status(exam, Timestamp::now()) {
        AttemptStatus::Submitted => Err(AppError::Conflict("the attempt is already submitted")),
        AttemptStatus::Expired => Err(AppError::Conflict("time is up — the attempt has expired")),
        AttemptStatus::InProgress => Ok(attempt),
    }
}

/// A 409 when the student has walked out of the exam room and the exam's
/// rejoin door is closed: no more answering (from anywhere) until the teacher
/// flips `allow_rejoin` back on. Finishing is deliberately exempt — see
/// `finish_attempt`.
pub(crate) fn check_rejoin(exam: &Exam, attempt: &ExamAttempt) -> Result<(), AppError> {
    if attempt.get_left_at().is_some() && !exam.get_allow_rejoin() {
        return Err(AppError::Conflict(
            "you left the exam and rejoin is closed — ask your teacher to reopen it",
        ));
    }
    Ok(())
}

/// Save one answer inside the caller's in-progress attempt — the whole write
/// path (attempt gate, rejoin gate, question lookup, kind check, upsert). The
/// REST handler saves into the latest sitting via this; the WebSocket room
/// resolves its own sitting first and shares [`save_answer_in`], so the two
/// can never drift.
pub(crate) async fn save_answer_checked(
    exam: &Exam,
    user: &UserId,
    question_id: &str,
    selected: Option<i64>,
    text: Option<String>,
    db: &Database,
) -> Result<ExamAnswer, AppError> {
    let attempt = writable_attempt(exam, user, db).await?;
    save_answer_in(exam, &attempt, question_id, selected, text, db).await
}

/// The tail of the answer write path, given the sitting to write in: the
/// enrollment wall (leaving the course closes the sheet, mid-exam included),
/// the rejoin gate, the question lookup, and the upsert.
pub(crate) async fn save_answer_in(
    exam: &Exam,
    attempt: &ExamAttempt,
    question_id: &str,
    selected: Option<i64>,
    text: Option<String>,
    db: &Database,
) -> Result<ExamAnswer, AppError> {
    ensure_enrolled(exam, attempt.get_user(), db).await?;
    check_rejoin(exam, attempt)?;
    let question = question_of_exam(exam.get_id(), question_id, db).await?;
    ExamAnswer::save(&question, attempt.get_user(), selected, text, db).await
}

/// A 403 unless `user` is enrolled in the exam's course — the same wall the
/// exam room checks at its door, re-applied to the sitting's content paths so
/// an unenrollment mid-exam cuts them too. Finishing stays exempt: like the
/// rejoin lock, submitting what's already saved writes nothing new.
pub(crate) async fn ensure_enrolled(
    exam: &Exam,
    user: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if Enrollment::read_for_user(exam.get_course(), user, db)
        .await?
        .is_none()
    {
        return Err(AppError::Forbidden(
            "you are not enrolled in this exam's course",
        ));
    }
    Ok(())
}

/// The exam's questions as the sitting student sees them: `correct` stripped,
/// their own saved answers embedded — the latest sitting's, since a retake
/// starts from a blank sheet. Requires enrollment in the exam's course (the
/// questions are course content — leaving the course closes them) and an
/// attempt — start one with `POST /exams/{id}/attempt` first (404 until
/// then). Readable in every attempt state, so a submitted student can still
/// review what they wrote.
#[utoipa::path(
    get,
    path = "/{id}/attempt/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "Questions with the caller's answers embedded", body = [AttemptQuestionResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
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
    ensure_enrolled(&exam, user.get_id(), &st.db).await?;
    // The question list is for sitting students; without an attempt there is
    // nothing to sit behind — and no early peek at the questions.
    ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db)
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
/// `choice` questions take `selected`; `text` questions take `text`. Requires
/// enrollment in the exam's course — an unenrollment mid-exam closes the
/// sheet. Rejected once the attempt is submitted or its deadline has passed —
/// the server clock, not the client's, is the judge — and rejected while the
/// student has left the exam room with the exam's rejoin door closed.
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
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or attempt", body = ErrorResponse),
        (status = 409, description = "Attempt already submitted, time is up, or rejoin is closed", body = ErrorResponse),
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

/// One student's answer sheet with correctness flags — always the *latest*
/// sitting's answers (a retake starts from a blank sheet). Every row carries
/// the saved answer plus `is_correct` (`null` for text questions — those are
/// the grader's call), and the machine's `auto_score` over the choice
/// questions is attached as a *suggestion*: the final mark stays human, via
/// `POST /exams/{id}/results`. Requires teacher+ and management rights over
/// the exam's course.
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
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or the student has no attempt", body = ErrorResponse),
    ),
)]
async fn attempt_answers(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can read answer sheets",
        ));
    }
    let target = UserId::from_key(&target);
    // No attempt means no answer sheet — a 404, not an empty one. Answers are
    // always the latest sitting's: a retake starts from a blank sheet.
    ExamAttempt::read_latest_for_user(exam.get_id(), &target, &st.db)
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
