use super::*;

use crate::domain::exam_attempt::AttemptStatus;
use crate::service::exam_attempt::{class_course_of, list_for_user, list_unfinished_for_user};
use crate::service::exam_question;

// ---- per-attempt history ----------------------------------------------------
// The grading views above show the latest sitting; these expose every prior
// sitting a re-taking student left behind. Same wall as grading: teacher+ who
// manages the exam's instance. A student never reaches another student's
// sheet, and a student's own prior attempts are staff-visible by design.

/// The exam plus the manage-rights check the grading and history reads share.
pub(crate) async fn gradable_exam(st: &AppState, user: &User, id: &str) -> Result<Exam, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    let instance = class_course_of(&exam, &st.db).await?;
    if !can_manage_instance(&st.db, instance.get_id(), user).await? {
        return Err(AppError::Forbidden(
            "only this instance's teachers, its class's homeroom teacher, or a manager/admin can read answer sheets",
        ));
    }
    Ok(exam)
}

/// The sitting numbers a student has left at an exam — every seq that carries
/// answers or a mark, ascending. Requires teacher+ and management rights over
/// the exam's instance. Drives the FE's attempt-by-attempt picker.
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
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
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
    let mut seqs =
        crate::service::exam_answer::list_seqs_for_user(&st.db, exam.get_id(), &target).await?;
    for result in
        crate::service::exam_result::list_all_for_exam_user(&st.db, exam.get_id(), &target).await?
    {
        seqs.push(result.get_seq());
    }
    seqs.sort_unstable();
    seqs.dedup();
    Ok(Json(seqs))
}

/// One prior sitting's judged answer sheet — the `seq`th attempt's answers,
/// drawing refs, correctness flags, and auto-score suggestion. Requires
/// teacher+ and management rights over the exam's instance. Serves an empty
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
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
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
/// rights over the exam's instance — the seq-scoped mirror of the grader's
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
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
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
    let question = exam_question::question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = crate::service::answer_image::read(&st.db, question.get_id(), &target, seq)
        .await?
        .ok_or(AppError::NotFound)?;
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}

/// A student's full mark history at an exam — every sitting's mark, oldest
/// first (the grade-of-record is the latest). Requires teacher+ and management
/// rights over the exam's instance.
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
        (status = 403, description = "Not this instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
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
    let results =
        crate::service::exam_result::list_all_for_exam_user(&st.db, exam.get_id(), &target).await?;
    let people = person_map(
        results
            .iter()
            .flat_map(|r| [*r.get_user(), *r.get_graded_by()]),
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
/// 404 if the exam is missing, still a draft, or the caller has no mark on it
/// (nothing to review yet), 403 if review is off for it, and 409 while the
/// caller could still write a sitting at it.
///
/// The mark check runs *before* the `allow_review` one, and must stay there: a
/// caller with no mark is an outsider to this exam, and answering them 403-if-
/// off / 404-otherwise made the status code an oracle for a flag they cannot
/// read anywhere else (`GET /exams/{id}` refuses them at `can_view_instance`
/// before it serves `allow_review` at all). With the mark first, an outsider's
/// answer is 404 whatever the flag says; only a caller who was marked — who
/// therefore sat the exam, enrolled or since dropped — ever sees the 403.
///
/// That last check is the one a mark alone can't make: the mark lookup reads the
/// *latest* sitting's mark, so a mark left at seq 1 satisfies it forever — a
/// student who starts a retake would otherwise read the answer key and their own
/// correctness flags while still writing seq 2. "Could still write" is the
/// *startable* test, not just the started one: a finished seq 1 on an exam whose
/// `max_attempts` still allows seq 2 leaks the key one `POST /attempt` before it
/// is used. So review opens only once the caller's sittings are used up (or the
/// exam's window has closed, or it was never sittable at all — a modeless,
/// offline-graded exam reviews as soon as the mark lands).
pub(crate) async fn reviewable_exam(
    st: &AppState,
    user: &User,
    id: &str,
) -> Result<Exam, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    if exam.is_draft() {
        return Err(AppError::NotFound);
    }
    crate::service::exam_result::read_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    if !exam.get_allow_review() {
        return Err(AppError::Forbidden("review not enabled for this exam"));
    }
    let attempts = list_for_user(&st.db, exam.get_id(), user.get_id()).await?;
    let now = Timestamp::now();
    if attempts
        .first()
        .is_some_and(|latest| latest.status(&exam, now) == AttemptStatus::InProgress)
    {
        return Err(AppError::Conflict(
            "finish your sitting before reviewing this exam",
        ));
    }
    // The same three conditions `POST /exams/{id}/attempt` starts a sitting
    // under (`ensure_sittable` + the window + `exam_attempt::start`'s limit
    // check), minus the caller's role and enrollment: those bar the sitting
    // without making the key any safer to hand out, and a student dropped from
    // the instance after being marked should still read their own review back.
    if exam.get_mode().is_some()
        && exam.get_ends_at().is_none_or(|ends| now < ends)
        && exam.get_max_attempts().allows_another(attempts.len())
    {
        return Err(AppError::Conflict(
            "you can sit this exam again — review opens once your attempts are used up or the exam ends",
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
/// ([`crate::service::exam_question::list_shared_with`]).
///
/// The open sittings are read whole and judged here rather than filtered in
/// SQL, because "in progress" is [`ExamAttempt::status`]'s call off the
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
    for attempt in list_unfinished_for_user(&st.db, user.get_id()).await? {
        let Some(other) = crate::service::exam::read(&st.db, attempt.get_exam()).await? else {
            continue;
        };
        if attempt.status(&other, now) == AttemptStatus::InProgress {
            live.push(other.get_id().clone());
        }
    }
    exam_question::list_shared_with(&st.db, exam.get_id(), &live).await
}

/// The caller's own sitting numbers at an exam — every seq that carries answers
/// or a mark, ascending. Own-scoped review view; opens once the teacher enables
/// review and has marked the caller, and closes again (409) while the caller can
/// still sit the exam.
#[utoipa::path(
    get,
    path = "/{id}/review/attempts",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The caller's own sitting numbers, ascending", body = [i64]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Review not enabled for this exam — only a caller who already has a mark on it ever reads this, everyone else gets the `404`", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
        (status = 409, description = "The caller can still sit this exam (a sitting in progress, or an attempt left under `max_attempts` while the exam is open)", body = ErrorResponse),
    ),
)]
pub(crate) async fn review_attempts(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<i64>>, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    let target = *user.get_id();
    let mut seqs =
        crate::service::exam_answer::list_seqs_for_user(&st.db, exam.get_id(), &target).await?;
    for result in
        crate::service::exam_result::list_all_for_exam_user(&st.db, exam.get_id(), &target).await?
    {
        seqs.push(result.get_seq());
    }
    seqs.sort_unstable();
    seqs.dedup();
    Ok(Json(seqs))
}

/// The exam's full question list, `correct` choice ids included — the answer key
/// the caller reviews their own sheet against. Same review gate as the other
/// self-review reads; revealing `correct` is the point (the gate already proves
/// the caller was marked and can no longer sit the exam). Paged via
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
        (status = 403, description = "Review not enabled for this exam — only a caller who already has a mark on it ever reads this, everyone else gets the `404`", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
        (status = 409, description = "The caller can still sit this exam (a sitting in progress, or an attempt left under `max_attempts` while the exam is open)", body = ErrorResponse),
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
/// review view; 409 while the caller can still sit the exam, so a retake can't
/// read its own correctness off an earlier seq.
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
        (status = 403, description = "Review not enabled for this exam — only a caller who already has a mark on it ever reads this, everyone else gets the `404`", body = ErrorResponse),
        (status = 404, description = "Exam not found, still a draft, or the caller has no mark on it", body = ErrorResponse),
        (status = 409, description = "The caller can still sit this exam (a sitting in progress, or an attempt left under `max_attempts` while the exam is open)", body = ErrorResponse),
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
/// sitting is still available.
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
        (status = 403, description = "Review not enabled for this exam — only a caller who already has a mark on it ever reads this, everyone else gets the `404`", body = ErrorResponse),
        (status = 404, description = "No such exam/question/drawing, a draft, or the caller has no mark on it", body = ErrorResponse),
        (status = 409, description = "The caller can still sit this exam (a sitting in progress, or an attempt left under `max_attempts` while the exam is open)", body = ErrorResponse),
    ),
)]
pub(crate) async fn review_attempt_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, seq, qid)): Path<(String, i64, String)>,
) -> Result<Response, AppError> {
    let exam = reviewable_exam(&st, &user, &id).await?;
    let question = exam_question::question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let image = crate::service::answer_image::read(&st.db, question.get_id(), user.get_id(), seq)
        .await?
        .ok_or(AppError::NotFound)?;
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}
