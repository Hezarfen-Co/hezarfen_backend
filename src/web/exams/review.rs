use super::*;

// ---- per-attempt history ----------------------------------------------------
// The grading views above show the latest sitting; these expose every prior
// sitting a re-taking student left behind. Same wall as grading: teacher+ who
// manages the exam's course. A student never reaches another student's sheet,
// and a student's own prior attempts are staff-visible by design.

/// The exam plus the manage-rights check the grading and history reads share.
pub(crate) async fn gradable_exam(st: &AppState, user: &User, id: &str) -> Result<Exam, AppError> {
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
pub(crate) async fn student_attempts(
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
pub(crate) async fn student_attempt_answers(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target, seq)): Path<(String, String, i64)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = gradable_exam(&st, &user, &id).await?;
    let target = UserId::from_key(&target);
    Ok(Json(
        answer_sheet(&exam, &target, seq, &HashSet::new(), &st.db).await?,
    ))
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
pub(crate) async fn student_attempt_answer_image(
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
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
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
pub(crate) async fn student_marks_history(
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
/// 404 if the exam is missing or still a draft, 403 if review is off for it,
/// 404 until the caller has a mark on it (nothing to review yet), and 409 while
/// the caller's latest sitting is still writable.
///
/// That last check is the one a mark alone can't make: the mark lookup reads the
/// *latest* sitting's mark, so a mark left at seq 1 satisfies it forever — a
/// student who starts a retake would otherwise read the answer key and their own
/// correctness flags while still writing seq 2. Only `InProgress` is refused;
/// an expired or submitted sitting reviews normally.
pub(crate) async fn reviewable_exam(
    st: &AppState,
    user: &User,
    id: &str,
) -> Result<Exam, AppError> {
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
    if let Some(attempt) =
        ExamAttempt::read_latest_for_user(exam.get_id(), user.get_id(), &st.db).await?
        && attempt.status(&exam, Timestamp::now()) == AttemptStatus::InProgress
    {
        return Err(AppError::Conflict(
            "finish your sitting before reviewing this exam",
        ));
    }
    Ok(exam)
}

/// The questions of `exam` whose answer key is live somewhere else right now:
/// the bank templates that were also copied into an exam the caller has a
/// sitting open on. The bank is copy-into-exam, so one template's `correct`
/// lands verbatim in every exam it is instantiated into — reviewing a graded
/// exam A would otherwise hand out the key to the very question the caller is
/// still answering in exam B.
///
/// Keyed on the template, never on the exam: a reviewed exam still reviews in
/// full, and only the overlapping questions blank out. Both link directions
/// count — a question authored by hand and *saved* to the bank shares its key
/// with every later instantiation just as an instantiated one does, so only a
/// question with no bank link at all is never hidden
/// ([`ExamQuestion::list_shared_with`]).
///
/// The open sittings are read whole and judged here rather than filtered in
/// SurrealQL, because "in progress" is [`ExamAttempt::status`]'s call off the
/// exam's *live* schedule and that rule lives in one place. One exam read per
/// unsubmitted sitting — a student has at most a handful, and none of it scales
/// with the page being read.
async fn live_elsewhere(
    st: &AppState,
    user: &User,
    exam: &Exam,
) -> Result<HashSet<String>, AppError> {
    let now = Timestamp::now();
    let mut live = Vec::new();
    for attempt in ExamAttempt::list_unfinished_for_user(user.get_id(), &st.db).await? {
        let Some(other) = Exam::read(attempt.get_exam(), &st.db).await? else {
            continue;
        };
        if attempt.status(&other, now) == AttemptStatus::InProgress {
            live.push(other.get_id().clone());
        }
    }
    ExamQuestion::list_shared_with(exam.get_id(), &live, &st.db).await
}

/// The caller's own sitting numbers at an exam — every seq that carries answers
/// or a mark, ascending. Own-scoped review view; opens once the teacher enables
/// review and has marked the caller, and closes again (409) while the caller has
/// a sitting in progress.
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
        (status = 409, description = "The caller's latest sitting is still in progress", body = ErrorResponse),
    ),
)]
pub(crate) async fn review_attempts(
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
/// the caller was marked and has no sitting still open). Paged via
/// `?limit=&offset=`.
///
/// One exception, per question: a question tied to a question-bank template —
/// added out of the bank, or saved into it — comes back with `correct: null`
/// while the caller has a sitting open on *another* exam holding a question
/// tied to that same template, since the copy would be that sitting's answer
/// key. The rest of the list is unaffected, and the `correct`
/// returns once that sitting is submitted or expires.
#[utoipa::path(
    get,
    path = "/{id}/review/questions",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id"), PageParams),
    responses(
        (status = 200, description = "A page of the exam's questions with `correct` (all of them when unpaged); `correct` is `null` on a question whose bank template the caller has live under an open sitting elsewhere", body = Page<QuestionResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Review not enabled for this exam", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
        (status = 409, description = "The caller's latest sitting is still in progress", body = ErrorResponse),
    ),
)]
pub(crate) async fn review_questions(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<QuestionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let exam = reviewable_exam(&st, &user, &id).await?;
    let hidden = live_elsewhere(&st, &user, &exam).await?;
    Ok(Json(
        question_page(&exam, limit, offset, &hidden, &st.db).await?,
    ))
}

/// One of the caller's own sittings, judged — the `seq`th attempt's answers,
/// drawing refs, correctness flags, and auto-score suggestion. Own-scoped
/// review view; 409 while the caller has a sitting in progress, so a retake
/// can't read its own correctness off an earlier seq.
///
/// A question the caller has live under an open sitting on another exam (same
/// bank template) comes back with `is_correct: null` and is left out of
/// `auto_score` — the same redaction `GET /{id}/review/questions` makes.
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
        (status = 409, description = "The caller's latest sitting is still in progress", body = ErrorResponse),
    ),
)]
pub(crate) async fn review_attempt_answers(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, seq)): Path<(String, i64)>,
) -> Result<Json<AttemptAnswersResponse>, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    let hidden = live_elsewhere(&st, &user, &exam).await?;
    Ok(Json(
        answer_sheet(&exam, user.get_id(), seq, &hidden, &st.db).await?,
    ))
}

/// The caller's own drawn-answer bytes for one of their sittings — the
/// seq-scoped, own-scoped mirror of the grader's drawing read. Same 409 while a
/// sitting is still open.
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
        (status = 409, description = "The caller's latest sitting is still in progress", body = ErrorResponse),
    ),
)]
pub(crate) async fn review_attempt_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, seq, qid)): Path<(String, i64, String)>,
) -> Result<Response, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = AnswerImage::read(question.get_id(), user.get_id(), seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}
