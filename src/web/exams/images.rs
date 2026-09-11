use super::*;

use crate::service::exam_attempt::{
    EXAM_LOCK, check_rejoin, course_of, ensure_enrolled, ensure_student, ensure_student_now,
    question_of_exam, read_latest_for_user, writable_attempt,
};

/// A stored question image's metadata; the bytes come from the image
/// endpoints (`GET .../image`, `GET .../choices/{choice_id}/image`).
#[derive(Serialize, ToSchema)]
pub(crate) struct ImageMetaResponse {
    /// MIME type as declared on upload (always one of the raster allowlist).
    #[schema(example = "image/png")]
    content_type: String,
    /// Image size in bytes.
    #[schema(example = 24_576)]
    size: i64,
}

impl ImageMetaResponse {
    pub(crate) fn new(image: &QuestionImage) -> Self {
        Self {
            content_type: image.get_content_type().as_str().to_string(),
            size: image.get_size(),
        }
    }

    pub(crate) fn from_answer(image: &AnswerImage) -> Self {
        Self {
            content_type: image.get_content_type().as_str().to_string(),
            size: image.get_size(),
        }
    }
}

/// The question's slot out of its image rows.
pub(crate) fn image_meta(
    images: &[QuestionImage],
    slot: Option<&ChoiceId>,
) -> Option<ImageMetaResponse> {
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
pub(crate) fn choice_image_metas(
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
pub(crate) async fn images_by_question(
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

// ---- question images ----------------------------------------------------------
// A question may carry one illustration (any kind — the map above the prompt)
// and, on choice questions, one picture per option (pick the right city off
// the map). Uploads are teacher authoring and freeze with the rest of the
// question once attempts exist; metadata rides on the question DTOs, bytes
// flow through the GET endpoints below, whose access follows the question's
// own visibility (author side and sitting side alike).

/// The exam, provided the caller may author its questions — the shared front
/// half of every image write. The archived-term refusal lives here rather than
/// in each caller: all four question-image writes (upload/delete of a question
/// illustration and of a choice picture) come through this one door, and none
/// of the reads do.
pub(crate) async fn image_managed_exam(
    st: &AppState,
    user: &User,
    id: &str,
) -> Result<Exam, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can manage question images",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    Ok(exam)
}

/// A 403/404 unless the caller may see the exam's question content: course
/// managers always, students through the same wall as
/// `GET /exams/{id}/attempt/questions` — enrollment plus a started attempt,
/// so there is no early peek at the pictures either.
pub(crate) async fn ensure_question_content_visible(
    st: &AppState,
    exam: &Exam,
    user: &User,
) -> Result<(), AppError> {
    let course = course_of(exam, &st.db).await?;
    if can_manage_course(&course, user) {
        return Ok(());
    }
    ensure_enrolled(exam, user.get_id(), &st.db).await?;
    read_latest_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(())
}

/// The image write tail, shared by both upload endpoints: the UPSERT replaces
/// the slot's row (the deterministic per-slot id makes it a replace) and names
/// the blob it retired *from inside its own transaction*, and [`store_blob`]
/// owns the disk ordering. Reading the slot out here first instead would hand
/// two uploads racing on one slot the same old blob name, leaving the loser's
/// fresh one on disk with no row pointing at it.
pub(crate) async fn store_image(
    st: &AppState,
    exam: &Exam,
    question: &ExamQuestion,
    slot: Option<&ChoiceId>,
    content_type: FileContentType,
    data: &[u8],
) -> Result<QuestionImage, AppError> {
    let image = QuestionImage::new(
        exam.get_id(),
        question.get_id(),
        slot,
        content_type,
        data.len() as i64,
    );
    let file = image.get_file().to_string();
    store_blob(st, &file, data, || async { image.upsert(&st.db).await }).await
}

/// The stored bytes, served inline via [`crate::web::serve_inline_blob`].
pub(crate) async fn serve_image(
    st: &AppState,
    image: &QuestionImage,
) -> Result<Response, AppError> {
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
pub(crate) async fn upload_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    // The body is consumed before the lock — a client's slow upload must not
    // stall the exam subsystem.
    let ImageUpload { content_type, data } = read_image_upload(&st, &mut multipart).await?;
    // No lock: the freeze gate is part of the image row's own transaction
    // (`QuestionImage::upsert`), and this path checks no subject. The
    // pre-flight below is the fast 409 and the byte-identical error.
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let stored = store_image(&st, &exam, &question, None, content_type, &data).await?;
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
pub(crate) async fn get_question_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn delete_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
pub(crate) async fn upload_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid, choice_id)): Path<(String, String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
    let ImageUpload { content_type, data } = read_image_upload(&st, &mut multipart).await?;
    ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    let slot = choice_slot(&question, &choice_id)?;
    let stored = store_image(&st, &exam, &question, Some(&slot), content_type, &data).await?;
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
pub(crate) async fn get_choice_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid, choice_id)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn delete_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid, choice_id)): Path<(String, String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = image_managed_exam(&st, &user, &id).await?;
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

// ---- answer images ----------------------------------------------------------
// A student's freehand drawing of their answer to a question — one per (exam,
// user, question), the answer-side mirror of the teacher's question images.
// It is a normal `image/png` (the frontend embeds its editable stroke JSON in a
// PNG `tEXt` chunk, opaque to us), so it flows through the same raster
// content-type wall and inline-serve path as every other exam image. The write
// paths ride the exact `save_answer` gate chain; the reads follow the question
// content's own visibility (own sitting view / teacher grading view).

/// The answer-image write tail, mirroring [`store_image`]: the deterministic
/// per-(question, user, seq) id makes the UPSERT a replace, the write names the
/// blob it retired from inside its own transaction, and [`store_blob`] owns the
/// disk ordering.
pub(crate) async fn store_answer_image(
    st: &AppState,
    exam: &Exam,
    question: &ExamQuestion,
    user: &UserId,
    seq: i64,
    content_type: FileContentType,
    data: &[u8],
) -> Result<AnswerImage, AppError> {
    let image = AnswerImage::new(
        exam.get_id(),
        question.get_id(),
        user,
        seq,
        content_type,
        data.len() as i64,
    );
    let file = image.get_file().to_string();
    store_blob(st, &file, data, || async { image.upsert(&st.db).await }).await
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
        (status = 409, description = "Attempt already submitted, time is up, rejoin is closed, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
pub(crate) async fn upload_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<ImageMetaResponse>), AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_student(&user)?;
    // Before the body is read: an archived term refuses the upload without
    // making the client push its bytes first.
    crate::service::course::require_open(&st.db, &course_of(&exam, &st.db).await?).await?;
    // The body is consumed before the lock — a client's slow upload must not
    // stall the exam subsystem (mirrors the question-image upload).
    let ImageUpload { content_type, data } = read_image_upload(&st, &mut multipart).await?;
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
        &data,
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
        (status = 409, description = "Attempt already submitted, time is up, rejoin is closed, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn delete_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_student(&user)?;
    crate::service::course::require_open(&st.db, &course_of(&exam, &st.db).await?).await?;
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
pub(crate) async fn get_answer_image(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, qid)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_question_content_visible(&st, &exam, &user).await?;
    let question = question_of_exam(exam.get_id(), &qid, &st.db).await?;
    // The caller's current sitting — the drawing belongs to their latest seq.
    let seq = read_latest_for_user(&st.db, exam.get_id(), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();
    let image = AnswerImage::read(question.get_id(), user.get_id(), seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
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
pub(crate) async fn get_student_answer_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target, qid)): Path<(String, String, String)>,
) -> Result<Response, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
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
    let seq = read_latest_for_user(&st.db, exam.get_id(), &target)
        .await?
        .ok_or(AppError::NotFound)?
        .get_seq();
    let image = AnswerImage::read(question.get_id(), &target, seq, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    crate::web::serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}
