use std::collections::HashMap;

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::database::Database;
use crate::domain::answer_image::AnswerImage;
use crate::domain::bank_question::{BankQuestion, BankQuestionId};
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::course::Course;
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamDuration, ExamId, ExamKind, ExamMode,
    ExamSchedule, ExamTitle,
};
use crate::domain::exam_answer::{ExamAnswer, auto_score};
use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt};
use crate::domain::exam_question::{
    Choice, ChoiceId, ExamQuestion, ExamQuestionId, QuestionKind, QuestionPoints, QuestionSpec,
    QuestionText,
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

use super::bank_questions::BankQuestionResponse;
use super::courses::{can_manage_course, can_view_course, visible_courses};
use super::subjects::subject_in_course;
use super::{
    ChoiceBody, CurrentUser, ExamResponse, Page, PageParams, PersonRef, RequireTeacher, Scheduled,
    UploadFileForm, WindowParams, blob_path, check_not_past, image_content_type, paginate,
    person_map, read_upload, remove_blob, set_or_clear,
};

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
struct ExamResultResponse {
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
    let exams = window.apply(exams)?;
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
        (status = 409, description = "Mode change after attempts started, or re-drafting an exam that has attempts or results", body = ErrorResponse),
    ),
)]
async fn update_exam(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateExam>,
) -> Result<Json<ExamResponse>, AppError> {
    // Take the writer lease before the row read: the merge defaults and the
    // mode/draft gates below all judge this snapshot, so a publish or mode
    // change landing between an unlocked read and the gates would be silently
    // written back over.
    let _guard = EXAM_LOCK.write().await;
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this exam",
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
    let allow_review = req.allow_review.unwrap_or_else(|| exam.get_allow_review());
    let draft = req.draft.unwrap_or_else(|| exam.is_draft());

    // Switching sync <-> async <-> open (or back to unscheduled) would
    // silently rewrite the deadline rules under students who already sat
    // down; extending times, the attempt limit, and the rejoin door are the
    // supported live adjustments instead. Gate read and write share the
    // handler-wide writer lease of [`EXAM_LOCK`], so a first attempt can't
    // land in the gap and leave a sat exam's mode flipped under it.
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
            allow_review,
            draft,
            &st.db,
        )
        .await?;
    Ok(Json(ExamResponse::new(&updated)))
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
            "only the course creator, an assigned teacher, or a manager/admin can delete this exam",
        ));
    }
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let images = QuestionImage::list_for_exam(exam.get_id(), &st.db).await?;
    let answer_images = AnswerImage::list_for_exam(exam.get_id(), &st.db).await?;
    exam.delete(&st.db).await?;
    for image in &images {
        remove_blob(&st.files_path, image.get_file()).await;
    }
    for image in &answer_images {
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin), or attempted to grade yourself", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can grade this exam",
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

    // The mark lands on the student's current sitting; the latest seq is the
    // grade-of-record. An offline-graded exam has no sitting — grade its base
    // seq (1).
    let seq = ExamAttempt::read_latest_for_user(&exam_id, &target, &st.db)
        .await?
        .map_or(1, |a| a.get_seq());
    let result = ExamResult::grade(&exam_id, &target, seq, mark, teacher.get_id(), &st.db).await?;
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
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
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
            "only the course creator, an assigned teacher, or a manager/admin can remove results",
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
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
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
    seq: i64,
    db: &Database,
) -> Result<(u64, u64), AppError> {
    let answered = ExamAnswer::list_for_exam_user(exam, user, seq, db)
        .await?
        .len() as u64;
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

    // A retake no longer wipes the prior sitting — each attempt's answers,
    // drawings, and marks stay put at their own seq (per-attempt history), so
    // there are no orphaned blobs to GC here. The exam-delete cascade still
    // cleans every sitting's blobs.
    let (attempt, created) = ExamAttempt::start(&exam, user.get_id(), &st.db).await?;
    drop(guard);
    let mark = ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) =
        attempt_progress(exam.get_id(), user.get_id(), attempt.get_seq(), &st.db).await?;
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
    let (answered, question_count) =
        attempt_progress(exam.get_id(), user.get_id(), attempt.get_seq(), &st.db).await?;
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
    let (answered, question_count) =
        attempt_progress(exam.get_id(), user.get_id(), finished.get_seq(), &st.db).await?;
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
    // Per-student progress: answer count and the latest save instant. The
    // per-exam read now returns every sitting's rows across all students, so
    // each answer is scoped to that student's *current* sitting — matching its
    // seq to their latest attempt — or a re-sitting student's count would
    // double up their prior attempts.
    let mut progress: HashMap<String, (u64, i64)> = HashMap::new();
    for answer in ExamAnswer::list_for_exam(exam.get_id(), db).await? {
        let key = answer.get_user().key().to_string();
        if attempts.get(&key).map(ExamAttempt::get_seq) != Some(answer.get_seq()) {
            continue;
        }
        let entry = progress.entry(key).or_insert((0, i64::MIN));
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
/// over the exam's course. Poll it to keep a monitor up to date.
#[utoipa::path(
    get,
    path = "/{id}/live",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "Live snapshot", body = ExamLiveResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can monitor this exam",
        ));
    }
    Ok(Json(live_snapshot(&exam, &st.db).await?))
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
    #[schema(example = "What is 2 + 2?", max_length = 2000)]
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    /// This question's share of the auto-score, `1`–`100`.
    #[schema(example = 10, minimum = 1, maximum = 100)]
    points: i64,
    /// The options of a `choice` question (2–10 of them); omit for `text`.
    /// Each carries an `id` naming it within this payload — the server mints
    /// the stored ids and returns them.
    #[schema(min_items = 2, max_items = 10)]
    choices: Option<Vec<ChoiceBody>>,
    /// The `id` of the right option, as sent in `choices`; required for
    /// `choice`, absent for `text`.
    #[schema(example = "b")]
    correct: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateQuestion {
    /// Re-tag the question with another of the course's subjects. Omit to
    /// keep the current one — a question always has a subject, so there is no
    /// clearing it.
    subject_id: Option<String>,
    #[schema(max_length = 2000)]
    text: Option<String>,
    #[schema(minimum = 1, maximum = 100)]
    points: Option<i64>,
    /// `choice` or `text`. Switching kinds needs the other fields to follow:
    /// send `choices` + `correct` when moving to `choice`, explicit `null`s
    /// when moving to `text`.
    kind: Option<String>,
    /// Omit to keep the stored options; send `null` to drop them (text
    /// questions only). Send each option back with the `id` it was returned
    /// with to keep it — its picture rides along. Only options whose id is
    /// absent from the new list lose their picture, so reordering, renaming,
    /// and deleting one option leave the rest untouched.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<Vec<ChoiceBody>>, min_items = 2, max_items = 10)]
    choices: Option<Option<Vec<ChoiceBody>>>,
    /// The `id` of the right option. Omit to keep; `null` to clear (text
    /// questions only).
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    correct: Option<Option<String>>,
}

/// A stored question image's metadata; the bytes come from the image
/// endpoints (`GET .../image`, `GET .../choices/{choice_id}/image`).
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

    fn from_answer(image: &AnswerImage) -> Self {
        Self {
            content_type: image.get_content_type().as_str().to_string(),
            size: image.get_size(),
        }
    }
}

/// The question's slot out of its image rows.
fn image_meta(images: &[QuestionImage], slot: Option<&ChoiceId>) -> Option<ImageMetaResponse> {
    images
        .iter()
        .find(|image| image.get_slot() == slot)
        .map(ImageMetaResponse::new)
}

/// The per-choice metas, aligned position-for-position with `choices` (`None`
/// entries = that option has no picture); `None` whole for text questions.
/// A response-only projection: the pictures are *stored* against choice ids, so
/// this alignment is rebuilt from the current list on every read and can never
/// go stale.
fn choice_image_metas(
    question: &ExamQuestion,
    images: &[QuestionImage],
) -> Option<Vec<Option<ImageMetaResponse>>> {
    question.get_choices().map(|choices| {
        choices
            .iter()
            .map(|choice| image_meta(images, Some(choice.get_id())))
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

/// One option on the wire: its stable id and its text. Carries **no** answer
/// key — `correct` is a sibling field on the question, and only on the
/// author-facing DTO, so a student response cannot contain one by construction.
#[derive(Serialize, ToSchema)]
pub(crate) struct ChoiceResponse {
    /// Send this back in a PATCH to keep the option (and its picture).
    id: String,
    text: String,
}

impl ChoiceResponse {
    pub(crate) fn list(choices: Option<&[Choice]>) -> Option<Vec<Self>> {
        choices.map(|choices| {
            choices
                .iter()
                .map(|choice| Self {
                    id: choice.get_id().as_str().to_string(),
                    text: choice.get_text().as_str().to_string(),
                })
                .collect()
        })
    }
}

/// A question as its author sees it — including the `correct` option. Never
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
    /// The options with their stable ids (`choice` questions only).
    choices: Option<Vec<ChoiceResponse>>,
    /// The id of the right option (`choice` questions only).
    correct: Option<String>,
    /// The question's illustration, if one was uploaded (any kind).
    image: Option<ImageMetaResponse>,
    /// Per-option pictures, aligned with `choices` (`choice` questions only).
    choice_images: Option<Vec<Option<ImageMetaResponse>>>,
    /// The bank template this question was created from, if it was added out of
    /// the bank (`GET /bank-questions/{id}`).
    from_bank: Option<String>,
    /// The bank template most recently created by saving this question into the
    /// bank, if any — set only by `POST …/questions/{qid}/to-bank`. Null on a
    /// question that came *from* the bank and was never saved back.
    banked_as: Option<String>,
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
            choices: ChoiceResponse::list(question.get_choices()),
            correct: question.get_correct().map(|id| id.as_str().to_string()),
            image: image_meta(images, None),
            choice_images: choice_image_metas(question, images),
            from_bank: question.get_from_bank().map(|b| b.key().to_string()),
            banked_as: question.get_banked_as().map(|b| b.key().to_string()),
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

/// The question's option named by `choice_id` — a 400 for a text question or an
/// id the question doesn't have, so an option picture can only ever be
/// addressed through an option that exists.
fn choice_slot(question: &ExamQuestion, choice_id: &str) -> Result<ChoiceId, AppError> {
    let Some(choices) = question.get_choices() else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "choice_id",
            reason: "only choice questions take option pictures",
        }));
    };
    choices
        .iter()
        .find(|choice| choice.get_id().as_str() == choice_id)
        .map(|choice| choice.get_id().clone())
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "choice_id",
            reason: "must name one of the choices",
        }))
}

/// Add a question to an exam. Requires teacher+ and management rights over the
/// exam's course. `subject_id` must name one of the course's subjects
/// (`GET /courses/{id}/subjects`) — every question belongs to a subject.
/// `choice` questions carry 2–10 `choices` plus `correct` naming one of them by id; `text`
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can author questions",
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
    // No stored choices to match against on create, so every option is new and
    // every id is minted here.
    let spec = QuestionSpec::try_new(
        QuestionKind::try_new(&req.kind)?,
        ChoiceBody::into_inputs(req.choices),
        req.correct,
        &[],
    )?;
    let question = ExamQuestion::create(exam.get_id(), subject, text, points, spec, &st.db).await?;
    // A question is born imageless — uploads come after, against its id.
    Ok((
        StatusCode::CREATED,
        Json(QuestionResponse::new(&question, &[])),
    ))
}

/// The exam's question list, `correct` choice ids included — the answer key,
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can read the question list",
        ));
    }
    Ok(Json(question_page(&exam, limit, offset, &st.db).await?))
}

/// The paged answer-key list (`correct` included) shared by the teacher
/// `list_questions` read and the student `review_questions` read — the two
/// differ only in the access wall they run first.
async fn question_page(
    exam: &Exam,
    limit: Option<i64>,
    offset: i64,
    db: &Database,
) -> Result<Page<QuestionResponse>, AppError> {
    let questions = ExamQuestion::list_for_exam(exam.get_id(), db).await?;
    let images = images_by_question(exam.get_id(), db).await?;
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
    Ok(Page::new(items, total, limit, offset))
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can edit questions",
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
    // Omitting `choices` re-submits the stored options *with their ids*, so a
    // text-only edit keeps every identity (and every picture) untouched.
    let choices = match req.choices {
        Some(update) => ChoiceBody::into_inputs(update),
        None => ChoiceBody::from_stored(question.get_choices()),
    };
    let correct = match req.correct {
        Some(update) => update,
        None => question.get_correct().map(|id| id.as_str().to_string()),
    };
    let stored: Vec<Choice> = question.get_choices().unwrap_or_default().to_vec();
    let spec = QuestionSpec::try_new(kind, choices, correct, &stored)?;

    let updated = question.update(subject, text, points, spec, &st.db).await?;
    // Only the options that are actually *gone* lose their pictures: keyed by
    // choice id, an option that survives the edit keeps its image no matter
    // where it moved in the list. (This used to wipe every option picture
    // whenever the request so much as carried a `choices` key.)
    let keep: Vec<ChoiceId> = updated
        .get_choices()
        .unwrap_or_default()
        .iter()
        .map(|choice| choice.get_id().clone())
        .collect();
    for image in QuestionImage::delete_choices_not_in(updated.get_id(), &keep, &st.db).await? {
        remove_blob(&st.files_path, image.get_file()).await;
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can delete questions",
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

// ---- question bank bridge ---------------------------------------------------
// Two copy funnels between an exam's questions and the school-wide bank
// (`web/bank_questions`): instantiate a template into this exam, or save one of
// this exam's questions back into the bank. Both COPY the row and every image
// blob to fresh ids/files — the source side is never touched or shared.

#[derive(Deserialize, ToSchema)]
struct InstantiateFromBank {
    /// The subject to file the new question under — one of the exam's course's
    /// subjects (the same-course rule the bank row itself is exempt from).
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    subject_id: String,
}

/// Instantiate a bank template into this exam as a fresh question. Requires
/// teacher+, management rights over the exam's course, and a template the
/// caller may see (their own, or one published to the school) — anything else
/// is a 404. `subject_id` must
/// name one of the course's subjects — the template's own subject is origin
/// metadata and does not carry over. The template (and its blobs) stay
/// untouched; a full copy — text, points, spec, illustration, and option
/// pictures — lands under a new question id. Locked once attempts exist.
#[utoipa::path(
    post,
    path = "/{id}/questions/from-bank/{bid}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("bid" = String, Path, description = "Bank question id"),
    ),
    request_body = InstantiateFromBank,
    responses(
        (status = 201, description = "Question created from the template", body = QuestionResponse),
        (status = 400, description = "Unknown subject, or one from another course", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no bank template the caller may see", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn question_from_bank(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, bid)): Path<(String, String)>,
    Json(req): Json<InstantiateFromBank>,
) -> Result<(StatusCode, Json<QuestionResponse>), AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can author questions",
        ));
    }
    // Writer lease of [`EXAM_LOCK`]: the freeze gate, the subject check, and the
    // create are one unit — same reasoning as `create_question`.
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let subject = subject_in_course(&req.subject_id, course.get_id(), &st.db).await?;

    let template = BankQuestion::read(&BankQuestionId::from_key(&bid), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // A template the caller may not see is a 404, exactly as it is on the bank's
    // own routes — instantiating is a read of `correct`, and a 403 here would
    // confirm that someone else's private template exists under that id.
    if !super::bank_questions::can_see(&template, &user) {
        return Err(AppError::NotFound);
    }
    let question = ExamQuestion::create_from_bank(
        exam.get_id(),
        subject,
        template.get_text().clone(),
        template.get_points(),
        template.spec(),
        template.get_id().clone(),
        &st.db,
    )
    .await?;

    // Copy each of the template's image blobs to a fresh file under the new
    // question + same slot (file before row, via `store_image`). A missing
    // source blob is server-side damage, surfaced as a 500 — not silently lost.
    // The copy is all-or-nothing: any mid-loop failure rolls back the fresh
    // question row (its cascade drops the copied image rows) and the blobs
    // written so far, leaving the source and destination untouched.
    let sources = BankQuestionImage::list_for_question(template.get_id(), &st.db).await?;
    let mut copied: Vec<String> = Vec::new();
    for source in &sources {
        let step = async {
            let bytes = tokio::fs::read(blob_path(&st.files_path, source.get_file()))
                .await
                .map_err(|err| {
                    AppError::Internal(format!(
                        "missing blob for bank image {}: {err}",
                        source.get_file()
                    ))
                })?;
            store_image(
                &st,
                &exam,
                &question,
                source.get_slot(),
                source.get_content_type().clone(),
                &bytes,
            )
            .await
        };
        match step.await {
            Ok(image) => copied.push(image.get_file().to_string()),
            Err(err) => {
                for file in &copied {
                    remove_blob(&st.files_path, file).await;
                }
                let _ = question.delete(&st.db).await;
                return Err(err);
            }
        }
    }
    let images = QuestionImage::list_for_question(question.get_id(), &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(QuestionResponse::new(&question, &images)),
    ))
}

/// Re-copy a bank template's *current* content over the exam question that was
/// instantiated from it — the escape hatch for the divergence a deep copy
/// creates: fixing a typo in the template does not reach the copies, so this is
/// how a copy is brought back in line, explicitly and per question. Requires
/// teacher+, management rights over the exam's course, and a template the
/// caller may still see.
///
/// Replaces text, points, kind, choices, `correct`, the illustration, and the
/// option pictures with the template's; the question keeps its own id, its
/// exam, its `subject` (exam-course-scoped — the template's subject is
/// unrelated metadata) and its provenance links. Anything edited on the exam
/// copy since it was inserted is overwritten.
///
/// **Choice ids come from the template**, exactly as
/// [`question_from_bank`] mints them — the copy adopts the template's ids, so
/// its option pictures key off the same slots the template's do, and a
/// re-copy after a template edit re-lands the right picture on the right
/// option. A recorded `selected` can never be stranded by that: the freeze
/// below refuses the whole route once *any* attempt exists, and an
/// `exam_answer` only exists under an attempt — so at refresh time no answer
/// points at any choice id at all.
///
/// - No `from_bank` (never came from the bank, or its template was deleted and
///   the cascade cleared the link) → `400`; there is nothing to refresh from.
/// - Template gone or not visible to the caller → `404`, never a 403.
/// - Any attempt started → `409`, the same `ensure_questions_editable` freeze
///   every other question mutation answers to.
#[utoipa::path(
    post,
    path = "/{id}/questions/{qid}/refresh-from-bank",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "Question refreshed from its template", body = QuestionResponse),
        (status = 400, description = "The question did not come from the bank", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, no such question in it, or no bank template the caller may see", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn question_refresh_from_bank(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<Json<QuestionResponse>, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit questions",
        ));
    }
    // Writer lease of [`EXAM_LOCK`]: the freeze gate, the read, and the
    // overwrite are one unit — same reasoning as `update_question`, which this
    // is a canned variant of.
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;

    let Some(source) = question.get_from_bank().cloned() else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "question",
            reason: "this question did not come from a bank template",
        }));
    };
    let template = BankQuestion::read(&source, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !super::bank_questions::can_see(&template, &user) {
        return Err(AppError::NotFound);
    }

    // Every source blob is read up front, *before* anything is written: a
    // missing one is server-side damage and must surface as a 500 with the
    // question untouched, since a half-applied refresh has no old content left
    // to roll back to. Bounded — at most one illustration plus ten option
    // pictures, each under the school's file cap.
    let sources = BankQuestionImage::list_for_question(template.get_id(), &st.db).await?;
    let mut incoming: Vec<(&BankQuestionImage, Vec<u8>)> = Vec::with_capacity(sources.len());
    for source in &sources {
        let bytes = tokio::fs::read(blob_path(&st.files_path, source.get_file()))
            .await
            .map_err(|err| {
                AppError::Internal(format!(
                    "missing blob for bank image {}: {err}",
                    source.get_file()
                ))
            })?;
        incoming.push((source, bytes));
    }

    // The question's own subject stays: it is checked against the exam's
    // course, and the template's is origin metadata from anywhere in school.
    let subject = question.get_subject().clone();
    // `spec()` hands over the template's stored choices *with their ids* rather
    // than re-minting any — the same funnel `question_from_bank` uses.
    let updated = question
        .update(
            subject,
            template.get_text().clone(),
            template.get_points(),
            template.spec(),
            &st.db,
        )
        .await?;

    // Make the pictures match the template exactly: drop every slot the
    // template has no picture for (including the illustration, and every option
    // that is gone after the re-copy), then write the template's over the rest.
    // `store_image` upserts per slot, so a slot both sides have is replaced.
    let incoming_slots: Vec<Option<&ChoiceId>> =
        incoming.iter().map(|(image, _)| image.get_slot()).collect();
    for stale in QuestionImage::list_for_question(updated.get_id(), &st.db).await? {
        if incoming_slots.contains(&stale.get_slot()) {
            continue;
        }
        let file = stale.get_file().to_string();
        stale.delete(&st.db).await?;
        remove_blob(&st.files_path, &file).await;
    }
    for (source, bytes) in &incoming {
        store_image(
            &st,
            &exam,
            &updated,
            source.get_slot(),
            source.get_content_type().clone(),
            bytes,
        )
        .await?;
    }

    let images = QuestionImage::list_for_question(updated.get_id(), &st.db).await?;
    Ok(Json(QuestionResponse::new(&updated, &images)))
}

/// Save one of this exam's questions into the school-wide bank as a reusable
/// template. Requires teacher+ and management rights over the exam's course.
/// The caller becomes the template's owner; the question's subject rides along
/// as origin metadata. A full copy — text, points, spec, illustration, and
/// option pictures — lands under a new bank id. Provenance rides both ways: the
/// origin exam is recorded on the template as `source_exam`, and the exam
/// question's `banked_as` is pointed at the new template (a repeat save is
/// allowed and repoints it at the newest one). `from_bank` is left alone — it
/// records the other direction and a save never changes where a question came
/// from.
#[utoipa::path(
    post,
    path = "/{id}/questions/{qid}/to-bank",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 201, description = "Template saved to the bank", body = BankQuestionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no such question in it", body = ErrorResponse),
    ),
)]
async fn question_to_bank(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<(StatusCode, Json<BankQuestionResponse>), AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can save questions to the bank",
        ));
    }
    // Reader lease of [`BANK_LOCK`] across the question read and the insert: the
    // template adopts the question's subject, so without it a subject delete
    // could slip between the two and leave the fresh template pointing at a
    // subject its cascade had already swept (that cascade runs under BANK_LOCK's
    // writer lease, so holding the reader lease orders us either side of it).
    let _bank_guard = super::bank_questions::BANK_LOCK.read().await;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;

    // `create_from_exam` mints its own id and insert (the funnel), fed the
    // question's fields plus the origin exam it was saved off.
    let template = BankQuestion::create_from_exam(
        user.get_id().clone(),
        question.get_subject().clone(),
        question.get_text().clone(),
        question.get_points(),
        question.spec(),
        exam.get_id().clone(),
        &st.db,
    )
    .await?;

    // Copy every image blob (illustration + option pictures) to a fresh file
    // under the new bank row + same slot (file before row). All-or-nothing: a
    // mid-loop failure rolls back the fresh bank row (cascade drops the copied
    // image rows) and the blobs written so far; the source exam question stays
    // untouched.
    let sources = QuestionImage::list_for_question(question.get_id(), &st.db).await?;
    let mut copied: Vec<String> = Vec::new();
    for source in &sources {
        let step = async {
            let bytes = tokio::fs::read(blob_path(&st.files_path, source.get_file()))
                .await
                .map_err(|err| {
                    AppError::Internal(format!(
                        "missing blob for question image {}: {err}",
                        source.get_file()
                    ))
                })?;
            super::bank_questions::store_image(
                &st,
                template.get_id(),
                source.get_slot(),
                source.get_content_type().clone(),
                &bytes,
            )
            .await
        };
        match step.await {
            Ok(image) => copied.push(image.get_file().to_string()),
            Err(err) => {
                for file in &copied {
                    remove_blob(&st.files_path, file).await;
                }
                let _ = template.delete(&st.db).await;
                return Err(err);
            }
        }
    }
    // Provenance back-link, so a client can tell this question was already
    // banked: the question points at the template it was saved into, and a
    // repeat save repoints it at the newest one (never blocked). Written only
    // after the all-or-nothing copy above, so a rolled-back template can never
    // leave a dangling link. A failure here is logged and swallowed rather than
    // rolling the template back: the save is the user's actual work and a
    // template silently vanishing over a metadata write is worse than a
    // template with no back-link (the next save re-links it).
    let question_key = question.get_id().key().to_string();
    // The bank lease has done its job (the template is inserted, its subject
    // pinned); hand it back *before* taking the exam one, so this handler never
    // holds both — the subject delete goes EXAM_LOCK then BANK_LOCK, and taking
    // them the other way round here would be a deadlock.
    drop(_bank_guard);
    // Reader lease of [`EXAM_LOCK`] around the back-link, the one write this
    // handler makes to `exam_question`. `ExamQuestion::update` is a whole-row
    // save taken under `EXAM_LOCK.write()`; without this lease the link could
    // land between that handler's read and its write, and the pre-link snapshot
    // would be written straight back over it — the provenance silently lost.
    // Held around the link alone, never across the blob copy above.
    let _exam_guard = EXAM_LOCK.read().await;
    if let Err(err) = question
        .link_banked_as(template.get_id().clone(), &st.db)
        .await
    {
        tracing::warn!(
            "saved question {question_key} to the bank but could not link it back: {err}"
        );
    }

    let images = BankQuestionImage::list_for_question(template.get_id(), &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(BankQuestionResponse::new(&template, &images)),
    ))
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
            "only the course creator, an assigned teacher, or a manager/admin can manage question images",
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

/// The whole image write tail, shared by both upload endpoints: new blob to
/// disk, row UPSERT (the deterministic per-slot id makes it a replace), then
/// the replaced blob off disk. A failed row write takes the fresh blob back
/// out; a stored row always points at a real blob.
async fn store_image(
    st: &AppState,
    exam: &Exam,
    question: &ExamQuestion,
    slot: Option<&ChoiceId>,
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

/// The stored bytes, served inline via [`super::serve_inline_blob`].
async fn serve_image(st: &AppState, image: &QuestionImage) -> Result<Response, AppError> {
    super::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
/// Same form, limits, and rights as the question-image upload; `choice_id` is
/// the `id` carried on that choice, as returned in the question's `choices`
/// (not a position — an unknown id is a `400`). Replacing the question's
/// `choices` list drops all its option pictures — re-upload against the new
/// list.
#[utoipa::path(
    post,
    path = "/{id}/questions/{qid}/choices/{choice_id}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
        ("choice_id" = String, Path, description = "Choice id, as returned in the question's `choices`"),
    ),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = ImageMetaResponse),
        (status = 400, description = "Missing file field, empty file, a content type outside the image allowlist, a text question, or an unknown choice id", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, or no such question in it", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid, choice_id)): Path<(String, String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let slot = choice_slot(&question, &choice_id)?;
    let stored = store_image(
        &st,
        &exam,
        &question,
        Some(&slot),
        content_type,
        &upload.data,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ImageMetaResponse::new(&stored))))
}

/// One option's picture bytes. Same access wall as the question-image read.
#[utoipa::path(
    get,
    path = "/{id}/questions/{qid}/choices/{choice_id}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
        ("choice_id" = String, Path, description = "Choice id"),
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
    Path((id, qid, choice_id)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_question_content_visible(&st, &exam, &user).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let slot = choice_slot(&question, &choice_id)?;
    let image = QuestionImage::read_slot(question.get_id(), Some(&slot), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    serve_image(&st, &image).await
}

/// Remove one option's picture. Requires teacher+ and management rights over
/// the exam's course; frozen once attempts exist.
#[utoipa::path(
    delete,
    path = "/{id}/questions/{qid}/choices/{choice_id}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
        ("choice_id" = String, Path, description = "Choice id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or image", body = ErrorResponse),
        (status = 409, description = "Attempts have started — questions are frozen", body = ErrorResponse),
    ),
)]
async fn delete_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid, choice_id)): Path<(String, String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let _guard = EXAM_LOCK.write().await;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let slot = choice_slot(&question, &choice_id)?;
    let image = QuestionImage::read_slot(question.get_id(), Some(&slot), &st.db)
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
    /// The picked option's id (`choice` questions).
    selected: Option<String>,
    /// The typed answer (`text` questions).
    text: Option<String>,
    /// When this answer was last saved, UTC unix-milliseconds.
    updated_at: i64,
    /// The student's own drawn answer, if any — bytes at
    /// `GET /exams/{id}/attempt/answers/{qid}/image`.
    answer_image: Option<ImageMetaResponse>,
}

impl AnswerStateResponse {
    fn new(answer: &ExamAnswer, answer_image: Option<ImageMetaResponse>) -> Self {
        Self {
            selected: answer.get_selected().map(|id| id.as_str().to_string()),
            text: answer.get_text().map(|t| t.as_str().to_string()),
            updated_at: answer.get_updated_at().as_millis(),
            answer_image,
        }
    }
}

/// A question as the sitting student sees it: no `correct` choice id, their own
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
    /// The options with their ids — and deliberately *not* `correct`: this DTO
    /// leaks no answer key because it has no field to carry one, and
    /// `ChoiceResponse` carries none either.
    choices: Option<Vec<ChoiceResponse>>,
    /// The question's illustration, if any — bytes at
    /// `GET /exams/{id}/questions/{qid}/image`.
    image: Option<ImageMetaResponse>,
    /// Per-option pictures aligned with `choices`, if any — bytes at
    /// `GET /exams/{id}/questions/{qid}/choices/{choice_id}/image`.
    choice_images: Option<Vec<Option<ImageMetaResponse>>>,
    /// The caller's saved answer; `null` while unanswered.
    answer: Option<AnswerStateResponse>,
}

#[derive(Deserialize, ToSchema)]
struct SaveAnswer {
    /// The question being answered.
    question_id: String,
    /// The picked option's `id` — required for `choice` questions.
    selected: Option<String>,
    /// The typed answer — required for `text` questions (empty clears the draft).
    #[schema(max_length = 10000)]
    text: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct AnswerSavedResponse {
    question: String,
    /// The picked option's id.
    selected: Option<String>,
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
    selected: Option<String>,
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
    selected: Option<String>,
    text: Option<String>,
    db: &Database,
) -> Result<ExamAnswer, AppError> {
    ensure_student_now(attempt.get_user(), db).await?;
    ensure_enrolled(exam, attempt.get_user(), db).await?;
    check_rejoin(exam, attempt)?;
    let question = question_of_exam(exam.get_id(), question_id, db).await?;
    ExamAnswer::save(
        &question,
        attempt.get_user(),
        attempt.get_seq(),
        selected,
        text,
        db,
    )
    .await
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
    // nothing to sit behind — and no early peek at the questions. The embedded
    // answers are the *current* sitting's only, so the seq scopes the reads.
    let seq = ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();

    let questions = ExamQuestion::list_for_exam(exam.get_id(), &st.db).await?;
    let images = images_by_question(exam.get_id(), &st.db).await?;
    let answers: HashMap<String, ExamAnswer> =
        ExamAnswer::list_for_exam_user(exam.get_id(), user.get_id(), seq, &st.db)
            .await?
            .into_iter()
            .map(|answer| (answer.get_question().key().to_string(), answer))
            .collect();
    let answer_images: HashMap<String, AnswerImage> =
        AnswerImage::list_for_exam_user(exam.get_id(), user.get_id(), seq, &st.db)
            .await?
            .into_iter()
            .map(|image| (image.get_question().key().to_string(), image))
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
                    choices: ChoiceResponse::list(question.get_choices()),
                    image: image_meta(question_images, None),
                    choice_images: choice_image_metas(question, question_images),
                    answer: answers.get(question.get_id().key()).map(|answer| {
                        AnswerStateResponse::new(
                            answer,
                            answer_images
                                .get(question.get_id().key())
                                .map(ImageMetaResponse::from_answer),
                        )
                    }),
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
        selected: answer.get_selected().map(|id| id.as_str().to_string()),
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
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
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
            "only the course creator, an assigned teacher, or a manager/admin can read answer sheets",
        ));
    }
    let target = UserId::from_key(&target);
    // No attempt means no answer sheet — a 404, not an empty one. This grading
    // view shows the *latest* sitting; prior sittings live under the
    // per-attempt history endpoints.
    let seq = ExamAttempt::read_latest_for_user(exam.get_id(), &target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();
    Ok(Json(answer_sheet(&exam, &target, seq, &st.db).await?))
}

/// One sitting's judged answer sheet: the `seq`th attempt's answers, drawing
/// refs, correctness flags, and auto-score suggestion. Shared by the latest-
/// sitting grading view and the per-attempt history endpoint.
async fn answer_sheet(
    exam: &Exam,
    target: &UserId,
    seq: i64,
    db: &Database,
) -> Result<AttemptAnswersResponse, AppError> {
    let questions = ExamQuestion::list_for_exam(exam.get_id(), db).await?;
    let answers = ExamAnswer::list_for_exam_user(exam.get_id(), target, seq, db).await?;
    let answer_images: HashMap<String, AnswerImage> =
        AnswerImage::list_for_exam_user(exam.get_id(), target, seq, db)
            .await?
            .into_iter()
            .map(|image| (image.get_question().key().to_string(), image))
            .collect();
    let by_question: HashMap<&str, &ExamQuestion> = questions
        .iter()
        .map(|question| (question.get_id().key(), question))
        .collect();
    let (earned, possible) = auto_score(&questions, &answers);
    let people = person_map([target.clone()], db).await?;
    Ok(AttemptAnswersResponse {
        exam: exam.get_id().key().to_string(),
        user: PersonRef::resolve(&people, target),
        answers: answers
            .iter()
            .map(|answer| StudentAnswerResponse {
                question: answer.get_question().key().to_string(),
                selected: answer.get_selected().map(|id| id.as_str().to_string()),
                text: answer.get_text().map(|t| t.as_str().to_string()),
                updated_at: answer.get_updated_at().as_millis(),
                is_correct: by_question
                    .get(answer.get_question().key())
                    .and_then(|question| answer.is_correct(question)),
                answer_image: answer_images
                    .get(answer.get_question().key())
                    .map(ImageMetaResponse::from_answer),
            })
            .collect(),
        auto_score: AutoScoreResponse { earned, possible },
    })
}

/// One row of a student's answer sheet, as the grader sees it.
#[derive(Serialize, ToSchema)]
struct StudentAnswerResponse {
    question: String,
    /// The picked option's id.
    selected: Option<String>,
    text: Option<String>,
    /// When the answer was last saved, UTC unix-milliseconds.
    updated_at: i64,
    /// Whether `selected` hits the question's `correct`; `null` for text
    /// questions (the grader judges those).
    is_correct: Option<bool>,
    /// The student's drawn answer, if any — bytes at
    /// `GET /exams/{id}/attempts/{user}/answers/{qid}/image`.
    answer_image: Option<ImageMetaResponse>,
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

// ---- answer images ----------------------------------------------------------
// A student's freehand drawing of their answer to a question — one per (exam,
// user, question), the answer-side mirror of the teacher's question images.
// It is a normal `image/png` (the frontend embeds its editable stroke JSON in a
// PNG `tEXt` chunk, opaque to us), so it flows through the same raster
// content-type wall and inline-serve path as every other exam image. The write
// paths ride the exact `save_answer` gate chain; the reads follow the question
// content's own visibility (own sitting view / teacher grading view).

/// The answer-image write tail, mirroring [`store_image`]: new blob to disk,
/// row UPSERT (the deterministic per-(question, user) id makes it a replace),
/// then the replaced blob off disk. A failed row write takes the fresh blob
/// back out; a stored row always points at a real blob.
async fn store_answer_image(
    st: &AppState,
    exam: &Exam,
    question: &ExamQuestion,
    user: &UserId,
    seq: i64,
    content_type: FileContentType,
    data: &[u8],
) -> Result<AnswerImage, AppError> {
    let replaced = AnswerImage::read(question.get_id(), user, seq, &st.db).await?;
    let image = AnswerImage::new(
        exam.get_id(),
        question.get_id(),
        user,
        seq,
        content_type,
        data.len() as i64,
    );
    let path = blob_path(&st.files_path, image.get_file());
    tokio::fs::write(&path, data).await.map_err(|err| {
        AppError::Internal(format!("failed to store the answer image blob: {err}"))
    })?;
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

/// Attach (or replace) the caller's drawn answer to a question inside their
/// in-progress attempt. `multipart/form-data` with the drawing under a `file`
/// field; the declared content type must be `image/png`, `image/jpeg`,
/// `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the
/// school's `max_file_bytes`. Rides the exact `POST /exams/{id}/attempt/answers`
/// gate chain: the student role, an in-progress attempt, current enrollment, and
/// the rejoin door.
#[utoipa::path(
    post,
    path = "/{id}/attempt/answers/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Drawing stored", body = ImageMetaResponse),
        (status = 400, description = "Missing file field, empty file, or a content type outside the image allowlist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or attempt", body = ErrorResponse),
        (status = 409, description = "Attempt already submitted, time is up, or rejoin is closed", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_student(&user)?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    // The body is consumed before the lock — a client's slow upload must not
    // stall the exam subsystem (mirrors the question-image upload).
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    // Reader lease of [`EXAM_LOCK`], exactly like `save_answer_checked`: the
    // writable gate and the write are one unit, or a retake's wipe-and-create
    // (a writer) slips in between and this stale drawing lands on the fresh
    // blank sheet.
    let _guard = EXAM_LOCK.read().await;
    let attempt = writable_attempt(&exam, user.get_id(), &st.db).await?;
    ensure_student_now(attempt.get_user(), &st.db).await?;
    ensure_enrolled(&exam, attempt.get_user(), &st.db).await?;
    check_rejoin(&exam, &attempt)?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let seq = attempt.get_seq();
    let stored = store_answer_image(
        &st,
        &exam,
        &question,
        user.get_id(),
        seq,
        content_type,
        &upload.data,
    )
    .await?;
    // A drawing-only answer (drew, typed nothing) still needs an ExamAnswer row,
    // or the drawing never surfaces in the sitting/grading views — answer_image
    // rides the answer payload. Create a blank text answer when absent, without
    // clobbering typed text. Choice questions can't hold a blank answer (and the
    // UI only offers drawing on text questions), so they are skipped. Removing
    // the drawing later grooms this blank row away (see `delete_answer_image`).
    if question.get_kind().as_str() != "choice"
        && ExamAnswer::read(question.get_id(), user.get_id(), seq, &st.db)
            .await?
            .is_none()
    {
        ExamAnswer::save(
            &question,
            user.get_id(),
            seq,
            None,
            Some(String::new()),
            &st.db,
        )
        .await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(ImageMetaResponse::from_answer(&stored)),
    ))
}

/// Clear the caller's drawn answer to a question. Same writable-attempt gate
/// chain as the upload.
#[utoipa::path(
    delete,
    path = "/{id}/attempt/answers/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "No such exam, question, attempt, or drawing", body = ErrorResponse),
        (status = 409, description = "Attempt already submitted, time is up, or rejoin is closed", body = ErrorResponse),
    ),
)]
async fn delete_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_student(&user)?;
    let _guard = EXAM_LOCK.read().await;
    let attempt = writable_attempt(&exam, user.get_id(), &st.db).await?;
    ensure_student_now(attempt.get_user(), &st.db).await?;
    ensure_enrolled(&exam, attempt.get_user(), &st.db).await?;
    check_rejoin(&exam, &attempt)?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let seq = attempt.get_seq();
    let image = AnswerImage::read(question.get_id(), user.get_id(), seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let image = image.delete(&st.db).await?;
    remove_blob(&st.files_path, image.get_file()).await;
    // If the drawing was the whole answer (blank text, no choice — the row the
    // upload created for a drawing-only answer), drop it too so it stops counting
    // as answered. A typed answer keeps its row.
    if let Some(answer) = ExamAnswer::read(question.get_id(), user.get_id(), seq, &st.db).await? {
        let blank = answer.get_selected().is_none()
            && answer.get_text().is_none_or(|t| t.as_str().is_empty());
        if blank {
            ExamAnswer::delete(question.get_id(), user.get_id(), seq, &st.db).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The caller's own drawn-answer bytes. Same visibility wall as the sitting
/// question view — enrollment plus a started attempt (404 before that).
#[utoipa::path(
    get,
    path = "/{id}/attempt/answers/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "The drawing bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the exam's course", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or drawing — or no attempt yet", body = ErrorResponse),
    ),
)]
async fn get_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_question_content_visible(&st, &exam, &user).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    // The caller's current sitting — the drawing belongs to their latest seq.
    let seq = ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();
    let image = AnswerImage::read(question.get_id(), user.get_id(), seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    super::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}

/// One student's drawn-answer bytes, for the grader. Requires teacher+ and
/// management rights over the exam's course — the `attempt_answers` gate.
#[utoipa::path(
    get,
    path = "/{id}/attempts/{user}/answers/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "The student's drawing bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or drawing", body = ErrorResponse),
    ),
)]
async fn get_student_answer_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target, qid)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can read answer sheets",
        ));
    }
    let target = UserId::from_key(&target);
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    // The grader's default view is the latest sitting; per-attempt drawings
    // come from the history image endpoint.
    let seq = ExamAttempt::read_latest_for_user(exam.get_id(), &target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();
    let image = AnswerImage::read(question.get_id(), &target, seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    super::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}

// ---- per-attempt history ----------------------------------------------------
// The grading views above show the latest sitting; these expose every prior
// sitting a re-taking student left behind. Same wall as grading: teacher+ who
// manages the exam's course. A student never reaches another student's sheet,
// and a student's own prior attempts are staff-visible by design.

/// The exam plus the manage-rights check the grading and history reads share.
async fn gradable_exam(st: &AppState, user: &User, id: &str) -> Result<Exam, AppError> {
    let exam = Exam::read(&ExamId::from_key(id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can read answer sheets",
        ));
    }
    Ok(exam)
}

/// The sitting numbers a student has left at an exam — every seq that carries
/// answers or a mark, ascending. Requires teacher+ and management rights over
/// the exam's course. Drives the FE's attempt-by-attempt picker.
#[utoipa::path(
    get,
    path = "/{id}/students/{user}/attempts",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 200, description = "The student's sitting numbers, ascending", body = [i64]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn student_attempts(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<Vec<i64>>, AppError> {
    let exam = gradable_exam(&st, &user, &id).await?;
    let target = UserId::from_key(&target);
    let mut seqs = ExamAnswer::list_seqs_for_user(exam.get_id(), &target, &st.db).await?;
    for result in ExamResult::list_all_for_exam_user(exam.get_id(), &target, &st.db).await? {
        seqs.push(result.get_seq());
    }
    seqs.sort_unstable();
    seqs.dedup();
    Ok(Json(seqs))
}

/// One prior sitting's judged answer sheet — the `seq`th attempt's answers,
/// drawing refs, correctness flags, and auto-score suggestion. Requires
/// teacher+ and management rights over the exam's course. Serves an empty
/// sheet for a seq the student never wrote in.
#[utoipa::path(
    get,
    path = "/{id}/students/{user}/attempts/{seq}/answers",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
        ("seq" = i64, Path, description = "Sitting number (1, 2, …)"),
    ),
    responses(
        (status = 200, description = "That sitting's answers, judged", body = AttemptAnswersResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn student_attempt_answers(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target, seq)): Path<(String, String, i64)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = gradable_exam(&st, &user, &id).await?;
    let target = UserId::from_key(&target);
    Ok(Json(answer_sheet(&exam, &target, seq, &st.db).await?))
}

/// A prior sitting's drawn-answer bytes. Requires teacher+ and management
/// rights over the exam's course — the seq-scoped mirror of the grader's
/// latest-sitting drawing read.
#[utoipa::path(
    get,
    path = "/{id}/students/{user}/attempts/{seq}/answers/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
        ("seq" = i64, Path, description = "Sitting number (1, 2, …)"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "The student's drawing bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "No such exam, question, or drawing", body = ErrorResponse),
    ),
)]
async fn student_attempt_answer_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target, seq, qid)): Path<(String, String, i64, String)>,
) -> Result<Response, AppError> {
    let exam = gradable_exam(&st, &user, &id).await?;
    let target = UserId::from_key(&target);
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = AnswerImage::read(question.get_id(), &target, seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    super::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}

/// A student's full mark history at an exam — every sitting's mark, oldest
/// first (the grade-of-record is the latest). Requires teacher+ and management
/// rights over the exam's course.
#[utoipa::path(
    get,
    path = "/{id}/students/{user}/marks",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 200, description = "The student's per-sitting marks, oldest first", body = [ExamResultResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Exam not found", body = ErrorResponse),
    ),
)]
async fn student_marks_history(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<Vec<ExamResultResponse>>, AppError> {
    let exam = gradable_exam(&st, &user, &id).await?;
    let target = UserId::from_key(&target);
    let results = ExamResult::list_all_for_exam_user(exam.get_id(), &target, &st.db).await?;
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

// ---- student self-review ----------------------------------------------------
// The mirror of the teacher history reads above, but own-scoped: no `{user}`
// path param, so the target is always the caller — a student can never reach
// another student's sheet. Opens only once the teacher enables review AND has
// marked this student (an ExamResult row proves it).

/// The exam plus the per-student review gate the three self-review reads share.
/// 404 if the exam is missing or still a draft, 403 if review is off for it, and
/// 404 until the caller has a mark on it (nothing to review yet).
async fn reviewable_exam(st: &AppState, user: &User, id: &str) -> Result<Exam, AppError> {
    let exam = Exam::read(&ExamId::from_key(id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if exam.is_draft() {
        return Err(AppError::NotFound);
    }
    if !exam.get_allow_review() {
        return Err(AppError::Forbidden("review not enabled for this exam"));
    }
    ExamResult::read_for_user(exam.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(exam)
}

/// The caller's own sitting numbers at an exam — every seq that carries answers
/// or a mark, ascending. Own-scoped review view; opens once the teacher enables
/// review and has marked the caller.
#[utoipa::path(
    get,
    path = "/{id}/review/attempts",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The caller's own sitting numbers, ascending", body = [i64]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Review not enabled for this exam", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
    ),
)]
async fn review_attempts(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<i64>>, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    let target = user.get_id().clone();
    let mut seqs = ExamAnswer::list_seqs_for_user(exam.get_id(), &target, &st.db).await?;
    for result in ExamResult::list_all_for_exam_user(exam.get_id(), &target, &st.db).await? {
        seqs.push(result.get_seq());
    }
    seqs.sort_unstable();
    seqs.dedup();
    Ok(Json(seqs))
}

/// The exam's full question list, `correct` choice ids included — the answer key
/// the caller reviews their own sheet against. Same review gate as the other
/// self-review reads; revealing `correct` is the point (the gate already proves
/// the caller was marked). Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/review/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id"), PageParams),
    responses(
        (status = 200, description = "A page of the exam's questions with `correct` (all of them when unpaged)", body = Page<QuestionResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Review not enabled for this exam", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
    ),
)]
async fn review_questions(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<QuestionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let exam = reviewable_exam(&st, &user, &id).await?;
    Ok(Json(question_page(&exam, limit, offset, &st.db).await?))
}

/// One of the caller's own sittings, judged — the `seq`th attempt's answers,
/// drawing refs, correctness flags, and auto-score suggestion. Own-scoped
/// review view.
#[utoipa::path(
    get,
    path = "/{id}/review/attempts/{seq}/answers",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("seq" = i64, Path, description = "Sitting number (1, 2, …)"),
    ),
    responses(
        (status = 200, description = "That sitting's answers, judged", body = AttemptAnswersResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Review not enabled for this exam", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
    ),
)]
async fn review_attempt_answers(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, seq)): Path<(String, i64)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    Ok(Json(answer_sheet(&exam, user.get_id(), seq, &st.db).await?))
}

/// The caller's own drawn-answer bytes for one of their sittings — the
/// seq-scoped, own-scoped mirror of the grader's drawing read.
#[utoipa::path(
    get,
    path = "/{id}/review/attempts/{seq}/answers/{qid}/image",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("seq" = i64, Path, description = "Sitting number (1, 2, …)"),
        ("qid" = String, Path, description = "Question id"),
    ),
    responses(
        (status = 200, description = "The caller's drawing bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Review not enabled for this exam", body = ErrorResponse),
        (status = 404, description = "No such exam/question/drawing, a draft, or the caller has no mark on it", body = ErrorResponse),
    ),
)]
async fn review_attempt_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, seq, qid)): Path<(String, i64, String)>,
) -> Result<Response, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = AnswerImage::read(question.get_id(), user.get_id(), seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    super::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}
