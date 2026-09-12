use super::*;

use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt};
use crate::service::exam_attempt;

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
pub(crate) struct AttemptResponse {
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
    pub(crate) fn new(
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
pub(crate) async fn attempt_progress(
    exam: &ExamId,
    user: &UserId,
    seq: i64,
    db: &Database,
) -> Result<(u64, u64), AppError> {
    let answered = crate::service::exam_answer::list_for_exam_user(db, exam, user, seq)
        .await?
        .len() as u64;
    let question_count = crate::service::exam_question::list_for_exam(db, exam, None, 0)
        .await?
        .0
        .len() as u64;
    Ok((answered, question_count))
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
        (status = 409, description = "Unscheduled (offline-graded) exam, outside the window, no attempts remaining, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn start_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<AttemptResponse>), AppError> {
    let (exam, attempt, created) =
        exam_attempt::start_attempt(&st.db, &ExamId::from_key(&id), &user).await?;
    let mark = crate::service::exam_result::read_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) =
        attempt_progress(exam.get_id(), user.get_id(), attempt.get_seq(), &st.db).await?;
    let used = exam_attempt::attempts_used(&st.db, exam.get_id(), user.get_id()).await?;
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
pub(crate) async fn my_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<AttemptResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let attempt = exam_attempt::read_latest_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let mark = crate::service::exam_result::read_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) =
        attempt_progress(exam.get_id(), user.get_id(), attempt.get_seq(), &st.db).await?;
    let used = exam_attempt::attempts_used(&st.db, exam.get_id(), user.get_id()).await?;
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
        (status = 409, description = "Already submitted, the deadline passed, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn finish_attempt(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<AttemptResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let finished = exam_attempt::finish_attempt(&st.db, &exam, user.get_id()).await?;
    let mark = crate::service::exam_result::read_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .map(|r| r.get_mark());
    let (answered, question_count) =
        attempt_progress(exam.get_id(), user.get_id(), finished.get_seq(), &st.db).await?;
    let used = exam_attempt::attempts_used(&st.db, exam.get_id(), user.get_id()).await?;
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
pub(crate) struct LiveStudentResponse {
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
pub(crate) struct LiveCountsResponse {
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
pub(crate) struct ExamLiveResponse {
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
pub(crate) async fn live_snapshot(
    exam: &Exam,
    db: &Database,
) -> Result<ExamLiveResponse, AppError> {
    let now = Timestamp::now();
    let (roster, _) =
        crate::service::enrollment::list_for_course(db, exam.get_course(), None, 0).await?;
    // Per student: their latest sitting (the one the monitor shows) plus how
    // many they've used.
    let mut attempts: HashMap<String, ExamAttempt> = HashMap::new();
    let mut used: HashMap<String, u64> = HashMap::new();
    for attempt in exam_attempt::list_for_exam(db, exam.get_id()).await? {
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
    let marks: HashMap<String, i64> = crate::service::exam_result::list_for_exam(db, exam.get_id())
        .await?
        .iter()
        .map(|result| {
            (
                result.get_user().key().to_string(),
                result.get_mark().as_i64(),
            )
        })
        .collect();
    let question_count = crate::service::exam_question::list_for_exam(db, exam.get_id(), None, 0)
        .await?
        .0
        .len() as u64;
    // Per-student progress: answer count and the latest save instant. The
    // per-exam read now returns every sitting's rows across all students, so
    // each answer is scoped to that student's *current* sitting — matching its
    // seq to their latest attempt — or a re-sitting student's count would
    // double up their prior attempts.
    let mut progress: HashMap<String, (u64, i64)> = HashMap::new();
    for answer in crate::service::exam_answer::list_for_exam(db, exam.get_id()).await? {
        let key = answer.get_user().key().to_string();
        if attempts.get(&key).map(ExamAttempt::get_seq) != Some(answer.get_seq()) {
            continue;
        }
        let entry = progress.entry(key).or_insert((0, i64::MIN));
        entry.0 += 1;
        entry.1 = entry.1.max(answer.get_updated_at().as_millis());
    }
    let people = person_map(roster.iter().map(|e| *e.get_user()), db).await?;

    // Once the window closes the door is shut for good (starting answers
    // 409), so "hasn't started" hardens into "was absent". Open exams have
    // no window and never make that call.
    let window_over = exam.get_ends_at().is_some_and(|ends| now >= ends);
    let mut students: Vec<LiveStudentResponse> = roster
        .iter()
        .map(|enrollment| {
            let key = enrollment.get_user().key();
            let attempt = attempts.get(&key);
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
                attempts_used: used.get(&key).copied().unwrap_or(0),
                started_at: attempt.map(|a| a.get_started_at().as_millis()),
                finished_at: attempt
                    .and_then(|a| a.get_finished_at())
                    .map(|t| t.as_millis()),
                left_at: attempt.and_then(|a| a.get_left_at()).map(|t| t.as_millis()),
                deadline: deadline.map(|t| t.as_millis()),
                remaining_ms: (status == Some(AttemptStatus::InProgress))
                    .then(|| deadline.map(|d| (d.as_millis() - now.as_millis()).max(0)))
                    .flatten(),
                mark: marks.get(&key).copied(),
                answered: progress.get(&key).map_or(0, |p| p.0),
                last_activity: progress.get(&key).map(|p| p.1),
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
pub(crate) async fn exam_live(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ExamLiveResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = exam_attempt::course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can monitor this exam",
        ));
    }
    Ok(Json(live_snapshot(&exam, &st.db).await?))
}

// ---- answers ----------------------------------------------------------------
// Students answer inside their attempt; every save is an upsert stamped with
// the server clock. The WebSocket room (`/exams/{id}/attempt/ws`) drives the
// same paths below.

/// A student's own saved answer, embedded in their question view.
#[derive(Serialize, ToSchema)]
pub(crate) struct AnswerStateResponse {
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
    pub(crate) fn new(answer: &ExamAnswer, answer_image: Option<ImageMetaResponse>) -> Self {
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
pub(crate) struct AttemptQuestionResponse {
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
pub(crate) struct SaveAnswer {
    /// The question being answered.
    question_id: String,
    /// The picked option's `id` — required for `choice` questions.
    selected: Option<String>,
    /// The typed answer — required for `text` questions (empty clears the draft).
    #[schema(max_length = 10000)]
    text: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AnswerSavedResponse {
    question: String,
    /// The picked option's id.
    selected: Option<String>,
    text: Option<String>,
    /// Save instant by the server clock, UTC unix-milliseconds.
    updated_at: i64,
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
pub(crate) async fn attempt_questions(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<AttemptQuestionResponse>>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    exam_attempt::ensure_enrolled(&exam, user.get_id(), &st.db).await?;
    // The question list is for sitting students; without an attempt there is
    // nothing to sit behind — and no early peek at the questions. The embedded
    // answers are the *current* sitting's only, so the seq scopes the reads.
    let seq = exam_attempt::read_latest_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();

    let (questions, _) =
        crate::service::exam_question::list_for_exam(&st.db, exam.get_id(), None, 0).await?;
    let images = images_by_question(exam.get_id(), &st.db).await?;
    let answers: HashMap<String, ExamAnswer> =
        crate::service::exam_answer::list_for_exam_user(&st.db, exam.get_id(), user.get_id(), seq)
            .await?
            .into_iter()
            .map(|answer| (answer.get_question().key().to_string(), answer))
            .collect();
    let answer_images: HashMap<String, AnswerImage> =
        crate::service::answer_image::list_for_exam_user(&st.db, exam.get_id(), user.get_id(), seq)
            .await?
            .into_iter()
            .map(|image| (image.get_question().key().to_string(), image))
            .collect();
    Ok(Json(
        questions
            .iter()
            .map(|question| {
                let question_images = images
                    .get(question.get_id().key().as_str())
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
                    answer: answers.get(question.get_id().key().as_str()).map(|answer| {
                        AnswerStateResponse::new(
                            answer,
                            answer_images
                                .get(question.get_id().key().as_str())
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
/// student has left the exam room with the rejoin door closed.
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
        (status = 409, description = "Attempt already submitted, time is up, rejoin is closed, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
pub(crate) async fn save_answer(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SaveAnswer>,
) -> Result<Json<AnswerSavedResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let answer = exam_attempt::save_answer_checked(
        &st.db,
        &exam,
        &user,
        &req.question_id,
        req.selected,
        req.text,
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
pub(crate) async fn attempt_answers(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = exam_attempt::course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can read answer sheets",
        ));
    }
    let target = UserId::from_key(&target);
    // No attempt means no answer sheet — a 404, not an empty one. This grading
    // view shows the *latest* sitting; prior sittings live under the
    // per-attempt history endpoints.
    let seq = exam_attempt::read_latest_for_user(&st.db, exam.get_id(), &target)
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();
    Ok(Json(
        answer_sheet(&exam, &target, seq, &HashSet::new(), &st.db).await?,
    ))
}

/// One sitting's judged answer sheet: the `seq`th attempt's answers, drawing
/// refs, correctness flags, and auto-score suggestion. Shared by the latest-
/// sitting grading view and the per-attempt history endpoint.
///
/// `hidden` names the questions whose answer key must not leak out of this
/// sheet (empty for a teacher). Dropping them from the question list is the
/// whole redaction: their `is_correct` falls to `null` for want of a question,
/// and they leave `auto_score` entirely — a per-question score still standing
/// in the total is the same bit, arrived at by subtraction.
pub(crate) async fn answer_sheet(
    exam: &Exam,
    target: &UserId,
    seq: i64,
    hidden: &HashSet<String>,
    db: &Database,
) -> Result<AttemptAnswersResponse, AppError> {
    let (mut questions, _) =
        crate::service::exam_question::list_for_exam(db, exam.get_id(), None, 0).await?;
    questions.retain(|question| !hidden.contains(question.get_id().key().as_str()));
    let answers =
        crate::service::exam_answer::list_for_exam_user(db, exam.get_id(), target, seq).await?;
    let answer_images: HashMap<String, AnswerImage> =
        crate::service::answer_image::list_for_exam_user(db, exam.get_id(), target, seq)
            .await?
            .into_iter()
            .map(|image| (image.get_question().key().to_string(), image))
            .collect();
    let by_question: HashMap<String, &ExamQuestion> = questions
        .iter()
        .map(|question| (question.get_id().key(), question))
        .collect();
    let (earned, possible) = auto_score(&questions, &answers);
    let people = person_map([*target], db).await?;
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
                    .get(answer.get_question().key().as_str())
                    .and_then(|question| answer.is_correct(question)),
                answer_image: answer_images
                    .get(answer.get_question().key().as_str())
                    .map(ImageMetaResponse::from_answer),
            })
            .collect(),
        auto_score: AutoScoreResponse { earned, possible },
    })
}

/// One row of a student's answer sheet, as the grader sees it.
#[derive(Serialize, ToSchema)]
pub(crate) struct StudentAnswerResponse {
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
pub(crate) struct AutoScoreResponse {
    /// Points earned where `selected == correct`.
    earned: i64,
    /// Total points across the exam's choice questions.
    possible: i64,
}

/// A student's full answer sheet for grading.
#[derive(Serialize, ToSchema)]
pub(crate) struct AttemptAnswersResponse {
    exam: String,
    /// The student whose sheet this is.
    user: PersonRef,
    answers: Vec<StudentAnswerResponse>,
    /// The suggested score over choice questions — never the final mark.
    auto_score: AutoScoreResponse,
}
