use std::collections::{HashMap, HashSet};

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::database::Database;
use crate::domain::answer_image::AnswerImage;
use crate::domain::badge;

use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode, ExamTitle,
};
use crate::domain::exam_answer::{ExamAnswer, auto_score};
use crate::domain::exam_question::{
    Choice, ChoiceId, ExamQuestion, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::exam_result::{ExamResult, Mark};
use crate::domain::note_file::FileContentType;
use crate::domain::question_image::QuestionImage;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::service::exam_attempt::{EXAM_LOCK, course_of, read_latest_for_user};
use crate::state::AppState;

use super::bank_questions::BankQuestionResponse;
use super::courses::{can_manage_course, can_view_course, visible_courses};
use super::subjects::subject_in_course;
use super::{
    ChoiceBody, CurrentUser, ExamResponse, ImageUpload, Page, PageParams, PersonRef,
    RequireTeacher, Scheduled, UploadFileForm, WindowParams, blob_path, check_not_past, paginate,
    person_map, read_image_upload, remove_blob, set_or_clear, store_blob,
};

pub(crate) mod attempts;
pub(crate) mod images;
pub(crate) mod questions;
pub(crate) mod review;

pub(crate) use attempts::*;
pub(crate) use images::*;
pub(crate) use questions::*;
pub(crate) use review::*;

impl Scheduled for Exam {
    fn starts_at_ms(&self) -> Option<i64> {
        self.get_starts_at().map(|at| at.as_millis())
    }

    fn ends_at_ms(&self) -> Option<i64> {
        self.get_ends_at().map(|at| at.as_millis())
    }

    fn order_key(&self) -> &str {
        self.get_id().key()
    }
}

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
        .routes(routes!(create_question, list_questions))
        .routes(routes!(update_question, delete_question))
        .routes(routes!(question_from_bank))
        .routes(routes!(question_to_bank))
        .routes(routes!(question_refresh_from_bank))
        .routes(routes!(attempt_questions))
        .routes(routes!(save_answer))
        .routes(routes!(attempt_answers))
        .routes(routes!(student_attempts))
        .routes(routes!(student_attempt_answers))
        .routes(routes!(student_marks_history))
        .routes(routes!(review_questions))
        .routes(routes!(review_attempts))
        .routes(routes!(review_attempt_answers))
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
                .routes(routes!(
                    upload_answer_image,
                    get_answer_image,
                    delete_answer_image
                ))
                .routes(routes!(get_student_answer_image))
                .routes(routes!(student_attempt_answer_image))
                .routes(routes!(review_attempt_answer_image))
                .layer(DefaultBodyLimit::max(
                    MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
                )),
        )
}

#[derive(Deserialize, ToSchema)]
struct UpdateExam {
    #[schema(max_length = 200)]
    title: Option<String>,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// The assessment form — one of the school's exam kinds (`GET /settings`).
    /// Changing it re-weights the exam: the course average uses the kind's
    /// settings-configured weight. For that reason an exam that already carries
    /// marks keeps its kind (`409`) — those marks would silently re-weight,
    /// exactly what the settings' kind-removal guard refuses.
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
    #[schema(value_type = Option<i64>, minimum = 60000, maximum = 86400000)]
    duration_ms: Option<Option<i64>>,
    /// Attempt limit: `1`–`100`, or `0` for unlimited. Omit to keep. Editable
    /// live — raising it grants retakes on the spot; lowering it only blocks
    /// future starts.
    #[schema(minimum = 0, maximum = 100)]
    max_attempts: Option<i64>,
    /// Whether students who left the exam room may come back in. Omit to
    /// keep. Editable live — the teacher's door handle for the running room.
    allow_rejoin: Option<bool>,
    /// Whether students may review their graded attempt once results are out.
    /// Omit to keep. Editable live.
    allow_review: Option<bool>,
    /// `false` publishes a draft (students can now see and sit it); `true`
    /// pulls a published exam back into hiding — allowed only while nobody
    /// has attempted it and nothing is graded (`409` otherwise). Omit to keep.
    draft: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
struct GradeResult {
    /// The mark to record, `0`–`100`.
    #[schema(example = 85, minimum = 0, maximum = 100)]
    mark: i64,
    /// The student being graded.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    user_id: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ExamResultResponse {
    id: String,
    exam: String,
    /// The graded student.
    user: PersonRef,
    /// Which sitting this mark belongs to (1 = first attempt). Lets the grader
    /// jump from a mark to that sitting's answer sheet
    /// (`GET /exams/{id}/students/{user}/attempts/{seq}/answers`) — without it
    /// a marked retake's answers are unreachable from the mark.
    seq: i64,
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
            seq: result.get_seq(),
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

// ---- exams --------------------------------------------------------------
// Exams are created inside a course: `POST /courses/{id}/exams`.

/// List the exams visible to the caller: every exam for manager+, otherwise
/// the exams of the courses they created or are enrolled in — minus other
/// people's drafts (a draft shows only to its course's managers). Paged via
/// `?limit=&offset=` (omit `limit` for the full list); returns a
/// `{items, total, limit, offset}` envelope.
///
/// The optional `?starts_after=&ends_after=` schedule window (unix
/// milliseconds) applies *after* the visibility and draft filtering, narrows
/// the list to upcoming/unfinished exams and flips the order to ascending by
/// schedule — so `?ends_after=<now>&limit=20` returns the twenty *soonest*
/// exams rather than the twenty newest-created. Exams without a window
/// (no `mode`, or `open`) are excluded by either parameter.
#[utoipa::path(
    get,
    path = "/",
    tag = "exams",
    security(("session_cookie" = [])),
    params(WindowParams, PageParams),
    responses(
        (status = 200, description = "A page of the caller's visible exams (all of them when unpaged)", body = Page<ExamResponse>),
        (status = 400, description = "Invalid window, limit, or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_exams(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(window): Query<WindowParams>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ExamResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let exams = if user.get_role().at_least(Role::Manager) {
        service::exam::list_all(&st.db).await?
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
        let mut exams = service::exam::list_for_courses(&st.db, &ids).await?;
        exams.retain(|exam| !exam.is_draft() || managed.contains(&exam.get_course().key()));
        exams
    };
    let exams = window.apply(exams)?;
    let total = exams.len() as i64;
    // Paged in the web layer: the draft filter and the window are Rust.
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
///
/// Gate order is deliberate and must stay as written: a caller with no view of
/// the course is refused by the *view* gate (`403`) before the draft gate is
/// reached, so the `404` rule covers only callers who can see the course — a
/// demoted creator, for instance, gets the `403` unless they are enrolled.
/// Moving the draft check above the view check to "make the 404 universal"
/// would hand every unenrolled caller a probe for which exams exist.
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
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this exam",
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
        (status = 400, description = "Invalid fields, kind, attempt limit, or schedule (malformed window, duration exceeding the window, or newly set times in the past)", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Mode change after attempts started, re-drafting an exam that has attempts or results, a kind change on an exam that already carries marks, the exam kept changing under concurrent edits, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_exam(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateExam>,
) -> Result<Json<ExamResponse>, AppError> {
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this exam",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;

    // Field validation, in the order the request is judged — only values this
    // request sets are held to the rules (a stored kind survives list edits,
    // kept times may legitimately be past). The merge against the stored row,
    // the schedule re-validation as a unit, and the mode-freeze / re-draft /
    // kind gates are the workflow's own steps:
    // [`crate::service::exam::update`] re-derives them per retry round behind
    // the compare-and-set.
    let title = req.title.as_deref().map(ExamTitle::try_new).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(ExamDescription::try_new)
        .transpose()?;
    let kind = match &req.kind {
        // Only a kind this request sets is held to the current settings list —
        // a stored kind survives later list edits, like past times survive
        // the no-past rule.
        Some(kind) => Some(ExamKind::try_new(
            kind,
            service::settings::load(&st.db).await?.get_exam_kinds(),
        )?),
        None => None,
    };
    let mode = match &req.mode {
        Some(update) => Some(update.as_deref().map(ExamMode::try_new).transpose()?),
        None => None,
    };
    let starts_at = match req.starts_at {
        Some(update) => {
            let starts_at = update.map(Timestamp::from_millis);
            check_not_past("starts_at", starts_at)?;
            Some(starts_at)
        }
        None => None,
    };
    let ends_at = match req.ends_at {
        Some(update) => {
            let ends_at = update.map(Timestamp::from_millis);
            check_not_past("ends_at", ends_at)?;
            Some(ends_at)
        }
        None => None,
    };
    let duration_ms = match req.duration_ms {
        Some(update) => Some(update.map(ExamDuration::try_new).transpose()?),
        None => None,
    };
    let max_attempts = req
        .max_attempts
        .map(ExamAttemptLimit::try_new)
        .transpose()?;
    let patch = service::exam::ExamPatch {
        title,
        description,
        kind,
        mode,
        starts_at,
        ends_at,
        duration_ms,
        max_attempts,
        allow_rejoin: req.allow_rejoin,
        allow_review: req.allow_review,
        draft: req.draft,
    };
    let exam = service::exam::update(&st.db, exam.get_id(), &patch).await?;
    Ok(Json(ExamResponse::new(&exam)))
}

/// Delete an exam. Requires teacher+ and management rights over the exam's
/// course (its creator, or manager/admin). Cascades the exam's results,
/// attempts, questions, answers, and question + answer images (blobs included).
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_exam(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this exam",
        ));
    }
    // The workflow — the archived-term gate, the *writer* lease of
    // [`EXAM_LOCK`] across the cascade, blob-key collection, the delete — is
    // [`crate::service::exam::delete`]'s. Blob unlinking stays here because
    // only the web layer knows `files_path`.
    let outcome = service::exam::delete(&st.db, &exam).await?;
    for file in &outcome.image_files {
        remove_blob(&st.files_path, file).await;
    }
    for file in &outcome.answer_image_files {
        remove_blob(&st.files_path, file).await;
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin), or attempted to grade yourself", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
        (status = 409, description = "The exam is a draft, its kind has been removed from the school's settings, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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
    let exam = service::exam::read(&st.db, &exam_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &teacher) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can grade this exam",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // Pre-flight: `ExamResult::grade` re-makes this check inside the mark's own
    // transaction, so a re-draft landing after this read cannot leave a mark on
    // a hidden exam.
    if exam.is_draft() {
        return Err(crate::domain::exam_result::draft_error());
    }
    // The exam's kind must still be one the school offers. `kind_ref`'s retired
    // bit is what actually refuses the mark inside `ExamResult::grade`, and it
    // is a *different record* from the list — a settings PATCH moves both, so
    // anything that leaves them disagreeing (a rolled-back retirement, a hand
    // edit) would otherwise reopen grading under a kind nobody lists, which is
    // also a mark that averages at weight 1 forever. Both gates, same answer;
    // this one is a read, the counter's is the one that survives a race.
    let kind = exam.get_kind().as_str();
    if !service::settings::load(&st.db)
        .await?
        .get_exam_kinds()
        .iter()
        .any(|offered| offered.get_name() == kind)
    {
        return Err(crate::domain::exam_result::retired_kind_error(kind));
    }

    let mark = Mark::try_new(req.mark)?;
    let target = UserId::from_key(&req.user_id);

    // Grading never targets oneself — no grader, whatever their role, may
    // write their own mark.
    if &target == teacher.get_id() {
        return Err(AppError::Forbidden("grading yourself is not allowed"));
    }

    // Target user must exist.
    let Some(target_user) = crate::service::user::read(&st.db, &target).await? else {
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
    if service::enrollment::read_for_user(&st.db, exam.get_course(), &target)
        .await?
        .is_none()
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user is not enrolled in this course",
        }));
    }

    // The mark lands on the student's current sitting; the latest seq is the
    // grade-of-record. An offline-graded exam has no sitting — grade its base
    // seq (1).
    let seq = read_latest_for_user(&st.db, &exam_id, &target)
        .await?
        .map_or(1, |a| a.get_seq());
    let result = ExamResult::grade(
        &exam_id,
        &target,
        seq,
        mark,
        teacher.get_id(),
        exam.get_kind().as_str(),
        &st.db,
    )
    .await?;
    // Both sides of the grade moved a counter — the grader's `marks_given`,
    // the student's `high_mark` — so both are brought up to date. A badge is a
    // decoration on top of the mark: losing one to a transient database error
    // must never fail the grading, and the next counter move heals it.
    for person in [teacher.get_id(), &target] {
        if let Err(err) = badge::sync(person, &st.db).await {
            tracing::warn!("failed to sync badges for {}: {err}", person.key());
        }
    }
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can list results",
        ));
    }
    let results = ExamResult::list_for_exam(exam.get_id(), &st.db).await?;
    let total = results.len() as i64;
    // Join people onto the page alone — the lookup shrinks with the window.
    // Paged in the web layer: the list is deduped to the latest mark per
    // sitting pair in Rust.
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn remove_result(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can remove results",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    let removed = ExamResult::remove(
        exam.get_id(),
        &UserId::from_key(&target),
        exam.get_kind().as_str(),
        &st.db,
    )
    .await?;
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn exam_statistics(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ExamStatisticsResponse>, AppError> {
    // Exam must exist — a missing exam is a 404, not an empty statistic.
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can view statistics",
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
