use super::*;

use crate::service::exam_question;

// ---- questions --------------------------------------------------------------
// Teachers author the question list before the exam runs; it freezes the
// moment anyone starts an attempt, so every student sits the same exam.

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateQuestion {
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
pub(crate) struct UpdateQuestion {
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
pub(crate) struct QuestionResponse {
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
    pub(crate) fn new(question: &ExamQuestion, images: &[QuestionImage]) -> Self {
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
pub(crate) async fn create_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateQuestion>,
) -> Result<(StatusCode, Json<QuestionResponse>), AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can author questions",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // No lease: the subject check below is only a pre-flight for the message,
    // and the insert takes the subject's reference counter in the same breath —
    // a subject delete lands either wholly before it (400) or is refused. The
    // freeze gate rides inside the insert's own transaction.
    exam_question::ensure_questions_editable(exam.get_id(), &st.db).await?;

    let subject = service::subject::in_course(&st.db, &req.subject_id, course.get_id()).await?;
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
    let question =
        exam_question::create(&st.db, exam.get_id(), subject, text, points, spec).await?;
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
pub(crate) async fn list_questions(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<QuestionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can read the question list",
        ));
    }
    Ok(Json(
        question_page(&exam, limit, offset, &HashSet::new(), &st.db).await?,
    ))
}

/// The paged answer-key list (`correct` included) shared by the teacher
/// `list_questions` read and the student `review_questions` read — the two
/// differ only in the access wall they run first, and in `hidden`: the question
/// ids whose `correct` must come back `null` (empty for a teacher).
pub(crate) async fn question_page(
    exam: &Exam,
    limit: Option<i64>,
    offset: i64,
    hidden: &HashSet<String>,
    db: &Database,
) -> Result<Page<QuestionResponse>, AppError> {
    let (questions, total) = exam_question::list_for_exam(db, exam.get_id(), limit, offset).await?;
    let images = images_by_question(exam.get_id(), db).await?;
    let items = questions
        .iter()
        .map(|question| {
            let mut item = QuestionResponse::new(
                question,
                images
                    .get(question.get_id().key())
                    .map_or(&[][..], Vec::as_slice),
            );
            if hidden.contains(question.get_id().key()) {
                item.correct = None;
            }
            item
        })
        .collect();
    Ok(Page::new(items, total, limit, offset))
}

/// Edit a question. Requires teacher+ and management rights over the exam's
/// course. Omitted fields keep their value; `kind`/`choices`/`correct` are
/// re-validated as a unit, so a kind switch must bring the matching fields
/// along. `subject_id` re-tags within the course's subjects. Locked once
/// attempts exist. An omitted `subject_id` is filled from the stored row, so
/// any edit here — not just a re-tag — is refused with a `409` when someone
/// else moved the question's subject after the caller read it.
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
        (status = 409, description = "Attempts have started — questions are frozen, the subject the question was read on changed since — nothing was written, re-read and retry — or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
pub(crate) async fn update_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
    Json(req): Json<UpdateQuestion>,
) -> Result<Json<QuestionResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit questions",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // No lease — see `create_question`; a re-tag moves the subject's reference
    // counter, and the freeze gate rides in the update's transaction.
    exam_question::ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = exam_question::question_of_exam(exam.get_id(), &qid, &st.db).await?;

    let subject = match req.subject_id {
        Some(ref subject_id) => {
            service::subject::in_course(&st.db, subject_id, course.get_id()).await?
        }
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

    let updated = exam_question::update(&st.db, question, subject, text, points, spec).await?;
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
    for image in
        crate::service::question_image::delete_choices_not_in(&st.db, updated.get_id(), &keep)
            .await?
    {
        remove_blob(&st.files_path, image.get_file()).await;
    }
    let images =
        crate::service::question_image::list_for_question(&st.db, updated.get_id()).await?;
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn delete_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete questions",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // No lock: the freeze gate is part of the delete's own transaction, and
    // this path checks no subject. The pre-flight below is the fast 409.
    exam_question::ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = exam_question::question_of_exam(exam.get_id(), &qid, &st.db).await?;
    // Rows go first (the delete cascades them), blobs after — a crash in
    // between strands at worst an unreachable blob.
    let images =
        crate::service::question_image::list_for_question(&st.db, question.get_id()).await?;
    exam_question::delete(&st.db, question).await?;
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
pub(crate) struct InstantiateFromBank {
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
        (status = 409, description = "Attempts have started — questions are frozen, or this course's term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
pub(crate) async fn question_from_bank(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, bid)): Path<(String, String)>,
    Json(req): Json<InstantiateFromBank>,
) -> Result<(StatusCode, Json<QuestionResponse>), AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can author questions",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // No lease — same reasoning as `create_question`. The freeze gate rides in
    // the insert's transaction.
    exam_question::ensure_questions_editable(exam.get_id(), &st.db).await?;
    let subject = service::subject::in_course(&st.db, &req.subject_id, course.get_id()).await?;

    let template = BankQuestion::read(&BankQuestionId::from_key(&bid), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // A template the caller may not see is a 404, exactly as it is on the bank's
    // own routes — instantiating is a read of `correct`, and a 403 here would
    // confirm that someone else's private template exists under that id.
    if !crate::web::bank_questions::can_see(&template, &user) {
        return Err(AppError::NotFound);
    }
    let question = exam_question::create_from_bank(
        &st.db,
        exam.get_id(),
        subject,
        template.get_text().clone(),
        template.get_points(),
        template.spec(),
        template.get_id().clone(),
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
                let _ = exam_question::delete(&st.db, question).await;
                return Err(err);
            }
        }
    }
    let images =
        crate::service::question_image::list_for_question(&st.db, question.get_id()).await?;
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
        (status = 409, description = "Attempts have started — questions are frozen, the question's subject was re-tagged since the caller read it — nothing was written, re-read and retry — or this course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn question_refresh_from_bank(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<Json<QuestionResponse>, AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit questions",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // No lock: the question keeps its own subject here, so there is nothing to
    // pair with a subject delete, and the freeze gate rides in the overwrite's
    // transaction.
    exam_question::ensure_questions_editable(exam.get_id(), &st.db).await?;
    let question = exam_question::question_of_exam(exam.get_id(), &qid, &st.db).await?;

    let Some(source) = question.get_from_bank().cloned() else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "question",
            reason: "this question did not come from a bank template",
        }));
    };
    let template = BankQuestion::read(&source, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !crate::web::bank_questions::can_see(&template, &user) {
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
    let updated = exam_question::update(
        &st.db,
        question,
        subject,
        template.get_text().clone(),
        template.get_points(),
        template.spec(),
    )
    .await?;

    // Make the pictures match the template exactly: drop every slot the
    // template has no picture for (including the illustration, and every option
    // that is gone after the re-copy), then write the template's over the rest.
    // `store_image` upserts per slot, so a slot both sides have is replaced.
    let incoming_slots: Vec<Option<&ChoiceId>> =
        incoming.iter().map(|(image, _)| image.get_slot()).collect();
    for stale in crate::service::question_image::list_for_question(&st.db, updated.get_id()).await?
    {
        if incoming_slots.contains(&stale.get_slot()) {
            continue;
        }
        let file = stale.get_file().to_string();
        crate::service::question_image::delete(&st.db, stale).await?;
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

    let images =
        crate::service::question_image::list_for_question(&st.db, updated.get_id()).await?;
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
        (status = 409, description = "This course's term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
pub(crate) async fn question_to_bank(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, qid)): Path<(String, String)>,
) -> Result<(StatusCode, Json<BankQuestionResponse>), AppError> {
    let exam = crate::service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let course = course_of(&exam, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can save questions to the bank",
        ));
    }
    crate::service::course::require_open(&st.db, &course).await?;
    // No lease: `BANK_LOCK` is gone with the subject delete's writer lease, and
    // a template left holding a deleted subject reads as an empty
    // `subject_name` either way (see [`crate::web::bank_questions`]).
    let question = exam_question::question_of_exam(exam.get_id(), &qid, &st.db).await?;

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
    let sources =
        crate::service::question_image::list_for_question(&st.db, question.get_id()).await?;
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
            crate::web::bank_questions::store_image(
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
    // No exam lease around the back-link any more, and none is needed: this is a
    // single-column `UPDATE`, and the question row `UPDATE` no longer re-states
    // this column from a snapshot (it names the columns it writes), so a
    // question edit racing the link cannot revert it. That lease existed only to
    // order those two, and the ordering requirement is gone with the whole-row
    // save that created it.
    if let Err(err) =
        exam_question::link_banked_as(&st.db, question, template.get_id().clone()).await
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
