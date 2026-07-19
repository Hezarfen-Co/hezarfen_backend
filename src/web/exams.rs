use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::{Stream, StreamExt};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{
    EXAM_LIVE_STREAM_INTERVAL_SECS, MAX_MAX_FILE_BYTES, QUESTION_IMAGE_CONTENT_TYPES,
    UPLOAD_BODY_OVERHEAD_BYTES,
};
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
use crate::domain::note_file::FileContentType;
use crate::domain::question_image::QuestionImage;
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course, visible_courses};
use super::subjects::subject_in_course;
use super::{
    CurrentUser, ExamResponse, Page, PageParams, PersonRef, RequireTeacher, UploadFileForm,
    blob_path, check_not_past, paginate, person_map, read_upload, remove_blob, set_or_clear,
};

/// Serializes the exam subsystem's cross-record check-then-writes, which
/// `BEGIN…COMMIT` cannot (write skew) — same reasoning as `REGISTER_LOCK`.
/// Read side: the answer saves (REST and the exam room), holding the sheet
/// open from the writable-attempt gate through the upsert, and the grade
/// write (draft gate through the result upsert); these stay concurrent with
/// each other. Write side: attempt starts (the max-attempts count and the
/// retake's answer wipe) and the structural teacher writes whose 409 guards
/// read attempt, question, or result rows first — exam mode/schedule/draft
/// updates, question create/update/delete, subject delete. So a save can
/// never land on a sheet a retake just wiped, a first attempt can never
/// slip between a freeze-gate read and the write it was meant to freeze,
/// and a mark can never land on an exam mid-flight into hiding.
// ponytail: global RwLock, shard per-exam if save latency ever matters.
pub(crate) static EXAM_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

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
        // The image routes get their own HTTP body cap, like the note-file
        // ones: the server-wide hard ceiling plus multipart framing headroom.
        .merge(
            OpenApiRouter::new()
                .routes(routes!(
                    upload_question_image,
                    get_question_image,
                    delete_question_image
                ))
                .routes(routes!(
                    upload_choice_image,
                    get_choice_image,
                    delete_choice_image
                ))
                .layer(DefaultBodyLimit::max(
                    MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
                )),
        )
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
    /// to turn the exam back into an offline-graded one. Frozen once anyone
    /// has started an attempt. Switching to `open` requires clearing
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
    /// `false` publishes a draft (students can now see and sit it); `true`
    /// pulls a published exam back into hiding — allowed only while nobody
    /// has attempted it and nothing is graded (`409` otherwise). Omit to keep.
    draft: Option<bool>,
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
/// the exams of the courses they created or are enrolled in — minus other
/// people's drafts (a draft shows only to its course's managers). Paged via
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
        // Drafts show only where the caller manages the course (as its
        // creator — the manager+ path above already saw everything).
        let managed: Vec<&str> = courses
            .iter()
            .filter(|c| can_manage_course(c, &user))
            .map(|c| c.get_id().key())
            .collect();
        let mut exams = Exam::list_for_courses(&ids, &st.db).await?;
        exams.retain(|exam| !exam.is_draft() || managed.contains(&exam.get_course().key()));
        exams
    };
    let total = exams.len() as i64;
    let items = paginate(&exams, limit, offset)
        .iter()
        .map(ExamResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single exam by id. Visible to its course's enrolled users, the
/// course creator, and managers/admins — except drafts, which only the
/// course's managers see (everyone else gets a `404`, as if the exam doesn't
/// exist yet — because it doesn't, officially).
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
        (status = 404, description = "Not found (or a draft the caller may not see)", body = ErrorResponse),
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
    // A draft doesn't exist for anyone but its course's managers — 404, not
    // 403, so its existence never leaks to the students it's hidden from.
    if exam.is_draft() && !can_manage_course(&course, &user) {
        return Err(AppError::NotFound);
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
/// `draft: false` publishes a draft; `draft: true` re-hides an exam, but only
/// while it has no attempts and no results (`409` otherwise) — students never
/// lose sight of an exam they've already sat or been graded on.
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
        (status = 409, description = "Mode change after attempts started, or re-drafting an exam that has attempts or results", body = ErrorResponse),
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
    let draft = req.draft.unwrap_or_else(|| exam.is_draft());

    // Switching sync <-> async <-> open (or back to unscheduled) would
    // silently rewrite the deadline rules under students who already sat
    // down; extending times, the attempt limit, and the rejoin door are the
    // supported live adjustments instead. Gate read and write share one
    // writer lease of [`EXAM_LOCK`], so a first attempt can't land in the
    // gap and leave a sat exam's mode flipped under it.
    let _guard = EXAM_LOCK.write().await;
    let mode_changed =
        schedule.get_mode().map(ExamMode::as_str) != exam.get_mode().map(ExamMode::as_str);
    if mode_changed && ExamAttempt::any_for_exam(exam.get_id(), &st.db).await? {
        return Err(AppError::Conflict(
            "cannot change the exam mode after attempts have started",
        ));
    }
    // Re-drafting hides the exam — never out from under a student who already
    // sat it or holds a mark on it. Same writer lease: a first attempt or an
    // in-flight grade (readers) can't slip between this gate and the write.
    if draft && !exam.is_draft() {
        let sat = ExamAttempt::any_for_exam(exam.get_id(), &st.db).await?;
        let graded = !ExamResult::list_for_exam(exam.get_id(), &st.db)
            .await?
            .is_empty();
        if sat || graded {
            return Err(AppError::Conflict(
                "cannot turn a published exam back into a draft after attempts or results exist",
            ));
        }
    }

    let updated = exam
        .update(
            title,
            description,
            kind,
            schedule,
            max_attempts,
            allow_rejoin,
            draft,
            &st.db,
        )
        .await?;
    Ok(Json(ExamResponse::new(&updated)))
}

/// Delete an exam. Requires teacher+ and management rights over the exam's
/// course (its creator, or manager/admin). Cascades the exam's results,
/// attempts, questions, answers, and question images (blobs included).
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
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let images = QuestionImage::list_for_exam(exam.get_id(), &st.db).await?;
    exam.delete(&st.db).await?;
    for image in &images {
        remove_blob(&st.files_path, image.get_file()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- results ------------------------------------------------------------

/// Record (or overwrite) a student's mark for an exam. Requires teacher+ and
/// management rights over the exam's course; the target must be a student and
/// enrolled. Only students carry marks; students never grade — and nobody
/// grades themselves. A draft can't be graded (`409`) — a mark would point at
/// an exam its student can't see.
#[utoipa::path(
    post,
    path = "/{id}/results",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = GradeResult,
    responses(
        (status = 200, description = "Result recorded", body = ExamResultResponse),
        (status = 400, description = "Invalid mark, unknown user, user not a student, or not enrolled", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin), or attempted to grade yourself", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
        (status = 409, description = "The exam is a draft", body = ErrorResponse),
    ),
)]
async fn grade(
    State(st): State<AppState>,
    RequireTeacher(teacher): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<GradeResult>,
) -> Result<Json<ExamResultResponse>, AppError> {
    let exam_id = ExamId::from_key(&id);
    // Reader lease of [`EXAM_LOCK`] from the exam read through the result
    // write: the draft gate below must be judged against the same row the
    // mark lands under, or a concurrent re-draft (a writer, which checks for
    // results) could slip between them and leave a mark on a hidden exam.
    let _guard = EXAM_LOCK.read().await;
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
    if exam.is_draft() {
        return Err(AppError::Conflict(
            "this exam is a draft — publish it before grading",
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

    // Only students carry marks — the grade system is theirs alone. A stale
    // enrollment left behind by a promotion can't reopen grading for staff.
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be graded",
        }));
    }

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

/// Rejects sitting an exam that can't be sat. A draft is a `404`, not a
/// `409` — sitting is a student act, drafts are invisible to students, and a
/// state-specific error would leak the existence this feature hides. A
/// modeless (offline-graded) exam is a `409`: visible, just nothing to sit.
/// Enrollment and window checks for the caller are the caller's own state —
/// also `Conflict`, not validation.
pub(crate) fn ensure_sittable(exam: &Exam) -> Result<(), AppError> {
    if exam.is_draft() {
        return Err(AppError::NotFound);
    }
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

/// Start, resume, or retake the caller's attempt. Requires the student role
/// (staff run exams, they don't sit them), enrollment in the exam's course, a
/// sittable exam (`sync`/`async`/`open` mode), and — when a window exists — the
/// window to be open. A still-running attempt is returned
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
        (status = 403, description = "Not a student, or not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "Exam not found (drafts are invisible here)", body = ErrorResponse),
        (status = 409, description = "Unscheduled (offline-graded) exam, outside the window, or no attempts remaining", body = ErrorResponse),
    ),
)]
async fn start_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<AttemptResponse>), AppError> {
    // Writer lease of [`EXAM_LOCK`] from the exam read through the start: the
    // sittable/window gates must be judged against the same exam row the
    // attempt lands under (the mirror of `update_exam`'s re-derive — without
    // it, a mode change or re-draft at legally-zero attempts could slip
    // between this gate and the insert, leaving an attempt on an unsittable
    // exam). The lease
    // also keeps the max-attempts count and the retake's wipe-and-create
    // from interleaving with an in-flight answer save (a reader). Dropped
    // before the response reads — they only describe the row.
    let guard = EXAM_LOCK.write().await;
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_sittable(&exam)?;
    ensure_student(&user)?;
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
    drop(guard);
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
    /// The subject this question belongs to — one of the exam's course's
    /// subjects (`GET /courses/{id}/subjects`). Required.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    subject_id: String,
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
    /// Re-tag the question with another of the course's subjects. Omit to
    /// keep the current one — a question always has a subject, so there is no
    /// clearing it.
    subject_id: Option<String>,
    text: Option<String>,
    points: Option<i64>,
    /// `choice` or `text`. Switching kinds needs the other fields to follow:
    /// send `choices` + `correct` when moving to `choice`, explicit `null`s
    /// when moving to `text`.
    kind: Option<String>,
    /// Omit to keep the stored options; send `null` to drop them (text
    /// questions only). Replacing or clearing the list also drops every
    /// option picture — the old images belong to the old options.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<Vec<String>>)]
    choices: Option<Option<Vec<String>>>,
    /// Omit to keep; `null` to clear (text questions only).
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<i64>)]
    correct: Option<Option<i64>>,
}

/// A stored question image's metadata; the bytes come from the image
/// endpoints (`GET .../image`, `GET .../choices/{index}/image`).
#[derive(Serialize, ToSchema)]
struct ImageMetaResponse {
    /// MIME type as declared on upload (always one of the raster allowlist).
    #[schema(example = "image/png")]
    content_type: String,
    /// Image size in bytes.
    #[schema(example = 24_576)]
    size: i64,
}

impl ImageMetaResponse {
    fn new(image: &QuestionImage) -> Self {
        Self {
            content_type: image.get_content_type().as_str().to_string(),
            size: image.get_size(),
        }
    }
}

/// The question's slot out of its image rows.
fn image_meta(images: &[QuestionImage], slot: Option<i64>) -> Option<ImageMetaResponse> {
    images
        .iter()
        .find(|image| image.get_slot() == slot)
        .map(ImageMetaResponse::new)
}

/// The per-choice metas, aligned index-for-index with `choices` (`None`
/// entries = that option has no picture); `None` whole for text questions.
fn choice_image_metas(
    question: &ExamQuestion,
    images: &[QuestionImage],
) -> Option<Vec<Option<ImageMetaResponse>>> {
    question.get_choices().map(|choices| {
        (0..choices.len() as i64)
            .map(|index| image_meta(images, Some(index)))
            .collect()
    })
}

/// `exam`'s image rows bucketed by question key — one query feeding a whole
/// question list.
async fn images_by_question(
    exam: &ExamId,
    db: &Database,
) -> Result<HashMap<String, Vec<QuestionImage>>, AppError> {
    let mut buckets: HashMap<String, Vec<QuestionImage>> = HashMap::new();
    for image in QuestionImage::list_for_exam(exam, db).await? {
        buckets
            .entry(image.get_question().key().to_string())
            .or_default()
            .push(image);
    }
    Ok(buckets)
}

/// A question as its author sees it — including the `correct` index. Never
/// serialized to students; they get [`AttemptQuestionResponse`].
#[derive(Serialize, ToSchema)]
struct QuestionResponse {
    id: String,
    exam: String,
    /// The subject this question belongs to (`GET /subjects/{id}`).
    subject: String,
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    points: i64,
    choices: Option<Vec<String>>,
    /// Zero-based index of the right option (`choice` questions only).
    correct: Option<i64>,
    /// The question's illustration, if one was uploaded (any kind).
    image: Option<ImageMetaResponse>,
    /// Per-option pictures, aligned with `choices` (`choice` questions only).
    choice_images: Option<Vec<Option<ImageMetaResponse>>>,
}

impl QuestionResponse {
    fn new(question: &ExamQuestion, images: &[QuestionImage]) -> Self {
        Self {
            id: question.get_id().key().to_string(),
            exam: question.get_exam().key().to_string(),
            subject: question.get_subject().key().to_string(),
            text: question.get_text().as_str().to_string(),
            kind: question.get_kind().as_str().to_string(),
            points: question.get_points().as_i64(),
            choices: question
                .get_choices()
                .map(|choices| choices.iter().map(|c| c.as_str().to_string()).collect()),
            correct: question.get_correct(),
            image: image_meta(images, None),
            choice_images: choice_image_metas(question, images),
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
/// exam's course. `subject_id` must name one of the course's subjects
/// (`GET /courses/{id}/subjects`) — every question belongs to a subject.
/// `choice` questions carry 2–10 `choices` plus the `correct` index; `text`
/// questions carry neither. Locked once attempts exist.
#[utoipa::path(
    post,
    path = "/{id}/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = CreateQuestion,
    responses(
        (status = 201, description = "Question created", body = QuestionResponse),
        (status = 400, description = "Invalid text, kind, points, choices, or correct — or an unknown subject, or one from another course", body = ErrorResponse),
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
    // Writer lease of [`EXAM_LOCK`]: the freeze gate, the subject check, and
    // the create are one unit — no first attempt can slip past the gate, and
    // no subject delete can invalidate a subject this just validated.
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;

    let subject = subject_in_course(&req.subject_id, course.get_id(), &st.db).await?;
    let text = QuestionText::try_new(&req.text)?;
    let points = QuestionPoints::try_new(req.points)?;
    let spec = QuestionSpec::try_new(QuestionKind::try_new(&req.kind)?, req.choices, req.correct)?;
    let question = ExamQuestion::create(exam.get_id(), subject, text, points, spec, &st.db).await?;
    // A question is born imageless — uploads come after, against its id.
    Ok((
        StatusCode::CREATED,
        Json(QuestionResponse::new(&question, &[])),
    ))
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
    let images = images_by_question(exam.get_id(), &st.db).await?;
    let total = questions.len() as i64;
    let items = paginate(&questions, limit, offset)
        .iter()
        .map(|question| {
            QuestionResponse::new(
                question,
                images
                    .get(question.get_id().key())
                    .map_or(&[][..], Vec::as_slice),
            )
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Edit a question. Requires teacher+ and management rights over the exam's
/// course. Omitted fields keep their value; `kind`/`choices`/`correct` are
/// re-validated as a unit, so a kind switch must bring the matching fields
/// along. `subject_id` re-tags within the course's subjects. Locked once
/// attempts exist.
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
        (status = 400, description = "Invalid text, kind, points, choices, or correct — or an unknown subject, or one from another course", body = ErrorResponse),
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
    // Writer lease of [`EXAM_LOCK`]: the freeze gate, the subject check, and
    // the update are one unit — see `create_question`.
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;

    let subject = match req.subject_id {
        Some(ref subject_id) => subject_in_course(subject_id, course.get_id(), &st.db).await?,
        None => question.get_subject().clone(),
    };
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
    let choices_replaced = req.choices.is_some();
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

    let updated = question.update(subject, text, points, spec, &st.db).await?;
    // A replaced (or cleared) choice list orphans the old options' pictures —
    // drop them all; the question's own illustration stays. Uploads re-attach
    // against the new list.
    if choices_replaced {
        for image in QuestionImage::delete_choices_for(updated.get_id(), &st.db).await? {
            remove_blob(&st.files_path, image.get_file()).await;
        }
    }
    let images = QuestionImage::list_for_question(updated.get_id(), &st.db).await?;
    Ok(Json(QuestionResponse::new(&updated, &images)))
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
    // Writer lease of [`EXAM_LOCK`]: freeze gate + delete are one unit — a
    // first attempt slipping past the gate would sit an exam whose question
    // list forks from what `auto_score` later judges.
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let images = QuestionImage::list_for_question(question.get_id(), &st.db).await?;
    question.delete(&st.db).await?;
    for image in &images {
        remove_blob(&st.files_path, image.get_file()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- question images ----------------------------------------------------------
// A question may carry one illustration (any kind — the map above the prompt)
// and, on choice questions, one picture per option (pick the right city off
// the map). Uploads are teacher authoring and freeze with the rest of the
// question once attempts exist; metadata rides on the question DTOs, bytes
// flow through the GET endpoints below, whose access follows the question's
// own visibility (author side and sitting side alike).

/// The exam, provided the caller may author its questions — the shared front
/// half of every image write.
async fn image_managed_exam(st: &AppState, user: &User, id: &str) -> Result<Exam, AppError> {
    let exam = Exam::read(&ExamId::from_key(id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can manage question images",
        ));
    }
    Ok(exam)
}

/// A 403/404 unless the caller may see the exam's question content: course
/// managers always, students through the same wall as
/// `GET /exams/{id}/attempt/questions` — enrollment plus a started attempt,
/// so there is no early peek at the pictures either.
async fn ensure_question_content_visible(
    st: &AppState,
    exam: &Exam,
    user: &User,
) -> Result<(), AppError> {
    let course = course_of(exam, &st.db).await?;
    if can_manage_course(&course, user) {
        return Ok(());
    }
    ensure_enrolled(exam, user.get_id(), &st.db).await?;
    ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

/// The declared content type, held to the raster allowlist — SVG stays out
/// (it can script) since these bytes are rendered inline to whole classes.
fn image_content_type(raw: &str) -> Result<FileContentType, AppError> {
    let content_type = FileContentType::try_new(raw)?;
    if !QUESTION_IMAGE_CONTENT_TYPES.contains(&content_type.as_str()) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "content_type",
            reason: "must be image/png, image/jpeg, image/webp, or image/gif",
        }));
    }
    Ok(content_type)
}

/// The whole image write tail, shared by both upload endpoints: new blob to
/// disk, row UPSERT (the deterministic per-slot id makes it a replace), then
/// the replaced blob off disk. A failed row write takes the fresh blob back
/// out; a stored row always points at a real blob.
async fn store_image(
    st: &AppState,
    exam: &Exam,
    question: &ExamQuestion,
    slot: Option<i64>,
    content_type: FileContentType,
    data: &[u8],
) -> Result<QuestionImage, AppError> {
    let replaced = QuestionImage::read_slot(question.get_id(), slot, &st.db).await?;
    let image = QuestionImage::new(
        exam.get_id(),
        question.get_id(),
        slot,
        content_type,
        data.len() as i64,
    );
    let path = blob_path(&st.files_path, image.get_file());
    tokio::fs::write(&path, data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the image blob: {err}")))?;
    match image.upsert(&st.db).await {
        Ok(stored) => {
            if let Some(replaced) = replaced {
                remove_blob(&st.files_path, replaced.get_file()).await;
            }
            Ok(stored)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err)
        }
    }
}

/// The stored bytes as an inline-displayable response: the declared (and
/// allowlisted) content type, `nosniff`, and `no-store` — exam content has no
/// business in shared caches, and a replaced image must not linger.
async fn serve_image(st: &AppState, image: &QuestionImage) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(blob_path(&st.files_path, image.get_file()))
        .await
        .map_err(|err| {
            // The row exists but its blob doesn't — server-side damage (a
            // lost volume path), not a client 404.
            AppError::Internal(format!(
                "missing blob for question image {}: {err}",
                image.get_file()
            ))
        })?;
    let content_type = HeaderValue::from_str(image.get_content_type().as_str())
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

/// Attach (or replace) a question's illustration — any question kind may
/// carry one, e.g. the map the prompt asks about. `multipart/form-data` with
/// the image under a `file` field; the declared content type must be
/// `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only —
/// no SVG), the bytes at most the school's `max_file_bytes` (settings).
/// Requires teacher+ and management rights over the exam's course; frozen
/// once attempts exist, like every other question edit.
#[utoipa::path(
    post,
    path = "/{id}/questions/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = ImageMetaResponse),
        (status = 400, description = "Missing file field, empty file, or a content type outside the image allowlist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no such question in it", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    // The body is consumed before the lock — a client's slow upload must not
    // stall the exam subsystem.
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    // Writer lease of [`EXAM_LOCK`]: freeze gate + write are one unit, like
    // every other question edit — see `create_question`.
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let stored = store_image(&st, &exam, &question, None, content_type, &upload.data).await?;
    Ok((StatusCode::CREATED, Json(ImageMetaResponse::new(&stored))))
}

/// The question's illustration bytes. Course managers read anytime; students
/// through the same wall as the sitting view — enrollment plus a started
/// attempt (404 before that, like the question list itself).
#[utoipa::path(
    get,
    path = "/{id}/questions/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or image — or no attempt yet", body = ErrorResponse),
    ),
)]
async fn get_question_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_question_content_visible(&st, &exam, &user).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = QuestionImage::read_slot(question.get_id(), None, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    serve_image(&st, &image).await
}

/// Remove a question's illustration. Requires teacher+ and management rights
/// over the exam's course; frozen once attempts exist.
#[utoipa::path(
    delete,
    path = "/{id}/questions/{qid}/image",
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
        (status = 404, description = "No such exam, question, or image", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn delete_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = QuestionImage::read_slot(question.get_id(), None, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let image = image.delete(&st.db).await?;
    remove_blob(&st.files_path, image.get_file()).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Attach (or replace) one option's picture on a `choice` question — so the
/// options themselves can be images (four map crops, pick the right one).
/// Same form, limits, and rights as the question-image upload; `index` is the
/// option's zero-based position. Replacing the question's `choices` list
/// drops all its option pictures — re-upload against the new list.
#[utoipa::path(
    post,
    path = "/{id}/questions/{qid}/choices/{index}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
        ("index" = i64, Path, description = "Zero-based choice index"),
    ),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = ImageMetaResponse),
        (status = 400, description = "Missing file field, empty file, a content type outside the image allowlist, a text question, or an index past the choices", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no such question in it", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid, index)): Path<(String, String, i64)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let Some(choices) = question.get_choices() else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "index",
            reason: "only choice questions take option pictures",
        }));
    };
    if !(0..choices.len() as i64).contains(&index) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "index",
            reason: "must index one of the choices",
        }));
    }
    let stored = store_image(
        &st,
        &exam,
        &question,
        Some(index),
        content_type,
        &upload.data,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ImageMetaResponse::new(&stored))))
}

/// One option's picture bytes. Same access wall as the question-image read.
#[utoipa::path(
    get,
    path = "/{id}/questions/{qid}/choices/{index}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
        ("index" = i64, Path, description = "Zero-based choice index"),
    ),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or image — or no attempt yet", body = ErrorResponse),
    ),
)]
async fn get_choice_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid, index)): Path<(String, String, i64)>,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_question_content_visible(&st, &exam, &user).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = QuestionImage::read_slot(question.get_id(), Some(index), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    serve_image(&st, &image).await
}

/// Remove one option's picture. Requires teacher+ and management rights over
/// the exam's course; frozen once attempts exist.
#[utoipa::path(
    delete,
    path = "/{id}/questions/{qid}/choices/{index}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
        ("index" = i64, Path, description = "Zero-based choice index"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or image", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn delete_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid, index)): Path<(String, String, i64)>,
) -> Result<StatusCode, AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = QuestionImage::read_slot(question.get_id(), Some(index), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let image = image.delete(&st.db).await?;
    remove_blob(&st.files_path, image.get_file()).await;
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
    /// The subject this question belongs to (`GET /subjects/{id}`).
    subject: String,
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    points: i64,
    choices: Option<Vec<String>>,
    /// The question's illustration, if any — bytes at
    /// `GET /exams/{id}/questions/{qid}/image`.
    image: Option<ImageMetaResponse>,
    /// Per-option pictures aligned with `choices`, if any — bytes at
    /// `GET /exams/{id}/questions/{qid}/choices/{index}/image`.
    choice_images: Option<Vec<Option<ImageMetaResponse>>>,
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
    // Reader lease of [`EXAM_LOCK`]: the writable gate and the upsert are
    // one unit, or a retake's wipe-and-create (a writer) slips in between
    // and this stale save lands on the fresh blank sheet.
    let _guard = EXAM_LOCK.read().await;
    let attempt = writable_attempt(exam, user, db).await?;
    save_answer_in(exam, &attempt, question_id, selected, text, db).await
}

/// The tail of the answer write path, given the sitting to write in: the
/// student wall and the enrollment wall (a promotion out of `student` or an
/// unenrollment closes the sheet, mid-exam included), the rejoin gate, the
/// question lookup, and the upsert.
pub(crate) async fn save_answer_in(
    exam: &Exam,
    attempt: &ExamAttempt,
    question_id: &str,
    selected: Option<i64>,
    text: Option<String>,
    db: &Database,
) -> Result<ExamAnswer, AppError> {
    ensure_student_now(attempt.get_user(), db).await?;
    ensure_enrolled(exam, attempt.get_user(), db).await?;
    check_rejoin(exam, attempt)?;
    let question = question_of_exam(exam.get_id(), question_id, db).await?;
    ExamAnswer::save(&question, attempt.get_user(), selected, text, db).await
}

/// A 403 unless `user` is a student. Sitting an exam is a student action —
/// teachers and above run exams, they never take them — so the sit paths
/// (start, room, save) enforce it on the *current* role. Checking the live
/// role, not just enrollment, closes the gap a mid-exam promotion would open
/// and neutralizes any stale non-student enrollment. Reading one's own attempt
/// and finishing stay ungated: a non-student has no attempt to read, and
/// finishing only submits work already saved.
pub(crate) fn ensure_student(user: &User) -> Result<(), AppError> {
    if user.get_role() != Role::Student {
        return Err(AppError::Forbidden("only students can sit exams"));
    }
    Ok(())
}

/// The answer path's live edition of [`ensure_student`]: re-read the row and
/// judge the *current* role, exactly like the enrollment wall beside it. Both
/// save paths run through here (REST per request, the exam room per message),
/// so a promotion out of `student` mid-exam closes the sheet on the very next
/// save — the room's door check is not the last word for a socket that
/// outlives the role.
pub(crate) async fn ensure_student_now(user: &UserId, db: &Database) -> Result<(), AppError> {
    let user = User::read(user, db).await?.ok_or(AppError::Unauthorized)?;
    ensure_student(&user)
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
    let images = images_by_question(exam.get_id(), &st.db).await?;
    let answers: HashMap<String, ExamAnswer> =
        ExamAnswer::list_for_exam_user(exam.get_id(), user.get_id(), &st.db)
            .await?
            .into_iter()
            .map(|answer| (answer.get_question().key().to_string(), answer))
            .collect();
    Ok(Json(
        questions
            .iter()
            .map(|question| {
                let question_images = images
                    .get(question.get_id().key())
                    .map_or(&[][..], Vec::as_slice);
                AttemptQuestionResponse {
                    id: question.get_id().key().to_string(),
                    subject: question.get_subject().key().to_string(),
                    text: question.get_text().as_str().to_string(),
                    kind: question.get_kind().as_str().to_string(),
                    points: question.get_points().as_i64(),
                    choices: question
                        .get_choices()
                        .map(|choices| choices.iter().map(|c| c.as_str().to_string()).collect()),
                    image: image_meta(question_images, None),
                    choice_images: choice_image_metas(question, question_images),
                    answer: answers
                        .get(question.get_id().key())
                        .map(AnswerStateResponse::new),
                }
            })
            .collect(),
    ))
}

/// Save (or overwrite) one answer in the caller's in-progress attempt.
/// `choice` questions take `selected`; `text` questions take `text`. Requires
/// the student role and enrollment in the exam's course — an unenrollment (or a
/// promotion out of `student`) mid-exam closes the sheet. Rejected once the
/// attempt is submitted or its deadline has passed —
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
        (status = 403, description = "Not a student, or not enrolled in the exam's course", body = ErrorResponse),
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
    ensure_student(&user)?;
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
