//! The school question pool: students ask (optionally with a photo of the
//! problem), teacher+ approve, and every approved question is readable — and
//! answerable — by the whole school. Parents stay out (they observe reports,
//! they don't take part). Pending questions are visible only to their asker
//! and to teacher+ (the approval queue); approval freezes the content, so the
//! only edit ever needed post-approval is deletion — which is also how a
//! teacher rejects: there is no "rejected" state to manage. Solutions are the
//! opposite: unmoderated, so their author may edit the body and attach,
//! replace, or drop one photo at any time — moderation there is delete-only.

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::domain::pool_question::{
    POOL_QUESTION_STATUSES, PoolQuestion, PoolQuestionBody, PoolQuestionId, PoolQuestionTitle,
};
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::solution::{Solution, SolutionBody, SolutionId};
use crate::domain::user::User;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireStudent, RequireTeacher, UploadFileForm,
    blob_path, image_content_type, paginate, person_map, read_upload, remove_blob,
    serve_inline_blob,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(ask_question, list_questions))
        .routes(routes!(get_question, delete_question))
        .routes(routes!(approve_question))
        .routes(routes!(offer_solution, list_solutions))
        .routes(routes!(delete_solution, edit_solution))
        // The image routes get their own HTTP body cap, like the note-file
        // ones: the server-wide hard ceiling plus multipart framing headroom.
        .merge(
            OpenApiRouter::new()
                .routes(routes!(upload_image, get_image, delete_image))
                .routes(routes!(
                    upload_solution_image,
                    get_solution_image,
                    delete_solution_image
                ))
                .layer(DefaultBodyLimit::max(
                    MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
                )),
        )
}

/// A 404 unless `user` may see `question`: approved questions are
/// school-wide; a pending one exists only for its asker and for teacher+
/// (the approval queue) — everyone else must not even learn it exists.
fn ensure_visible(question: &PoolQuestion, user: &User) -> Result<(), AppError> {
    if question.is_approved()
        || user.get_role().at_least(Role::Teacher)
        || question.get_asker() == user.get_id()
    {
        return Ok(());
    }
    Err(AppError::NotFound)
}

/// The shared front half of the asker-only, pending-only writes (image
/// upload/removal): 404 invisible, 403 not the asker, 409 already approved.
fn ensure_asker_editable(question: &PoolQuestion, user: &User) -> Result<(), AppError> {
    ensure_visible(question, user)?;
    if question.get_asker() != user.get_id() {
        return Err(AppError::Forbidden(
            "only the asker may change the question's image",
        ));
    }
    if question.is_approved() {
        return Err(AppError::Conflict(
            "the question is approved — its content is frozen",
        ));
    }
    Ok(())
}

async fn question_or_404(st: &AppState, id: &str) -> Result<PoolQuestion, AppError> {
    PoolQuestion::read(&PoolQuestionId::from_key(id), &st.db)
        .await?
        .ok_or(AppError::NotFound)
}

#[derive(Deserialize, ToSchema)]
struct AskQuestion {
    #[schema(example = "Bu integrali çözemedim")]
    title: String,
    #[schema(example = "∫x·eˣ dx nasıl adım adım çözülür?")]
    body: String,
}

#[derive(Deserialize, ToSchema)]
struct OfferSolution {
    #[schema(example = "Kısmi integrasyon: u = x, dv = eˣ dx …")]
    body: String,
}

#[derive(Deserialize, IntoParams)]
struct PoolQuestionFilter {
    /// Narrow to one status: `pending` (for a student: own unapproved
    /// questions; for teacher+: the approval queue) or `approved` (the pool).
    /// Omit for both.
    #[param(example = "pending")]
    status: Option<String>,
}

/// A stored pool photo's metadata — a question's or a solution's; the bytes
/// come from `GET /questions/{id}/image` or
/// `GET /questions/{id}/solutions/{sid}/image`.
#[derive(Serialize, ToSchema)]
struct PoolImageMeta {
    #[schema(example = "image/jpeg")]
    content_type: String,
    /// Byte size of the stored image.
    #[schema(example = 204_800)]
    size: i64,
}

/// A pool question. `status` is `pending` (awaiting approval — visible only
/// to the asker and teacher+) or `approved` (in the school-wide pool).
#[derive(Serialize, ToSchema)]
struct PoolQuestionResponse {
    id: String,
    /// The student who asked.
    asker: PersonRef,
    title: String,
    body: String,
    #[schema(example = "approved")]
    status: String,
    /// When it was asked, UTC unix-milliseconds (server-stamped).
    asked_at: i64,
    /// Who approved it; `null` while pending.
    approved_by: Option<PersonRef>,
    /// The attached photo's metadata; `null` when there is none. Bytes at
    /// `GET /questions/{id}/image`.
    image: Option<PoolImageMeta>,
    /// How many solutions the question carries — so a list can show activity
    /// without a per-question round trip.
    solution_count: i64,
}

impl PoolQuestionResponse {
    fn new(
        question: &PoolQuestion,
        people: &std::collections::HashMap<String, PersonRef>,
        solution_count: i64,
    ) -> Self {
        Self {
            id: question.get_id().key().to_string(),
            asker: PersonRef::resolve(people, question.get_asker()),
            title: question.get_title().as_str().to_string(),
            body: question.get_body().as_str().to_string(),
            status: question.get_status().to_string(),
            asked_at: question.get_asked_at().as_millis(),
            approved_by: question
                .get_approved_by()
                .map(|approver| PersonRef::resolve(people, approver)),
            image: question
                .get_image_content_type()
                .map(|content_type| PoolImageMeta {
                    content_type: content_type.as_str().to_string(),
                    size: question.get_image_size().unwrap_or(0),
                }),
            solution_count,
        }
    }
}

/// One offered solution on a pool question.
#[derive(Serialize, ToSchema)]
struct SolutionResponse {
    id: String,
    /// The question this solution answers.
    question: String,
    /// Who offered it — any role, student to admin.
    author: PersonRef,
    body: String,
    /// When it was offered, UTC unix-milliseconds (server-stamped).
    offered_at: i64,
    /// The attached photo's metadata; `null` when there is none. Bytes at
    /// `GET /questions/{id}/solutions/{sid}/image`.
    image: Option<PoolImageMeta>,
}

impl SolutionResponse {
    fn new(solution: &Solution, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: solution.get_id().key().to_string(),
            question: solution.get_question().key().to_string(),
            author: PersonRef::resolve(people, solution.get_author()),
            body: solution.get_body().as_str().to_string(),
            offered_at: solution.get_offered_at().as_millis(),
            image: solution
                .get_image_content_type()
                .map(|content_type| PoolImageMeta {
                    content_type: content_type.as_str().to_string(),
                    size: solution.get_image_size().unwrap_or(0),
                }),
        }
    }
}

/// Join the askers, approvers, and solution tallies of a page of questions
/// into responses — both joins run over the page slice only, so a long pool
/// history never widens the queries.
async fn question_responses(
    questions: &[PoolQuestion],
    st: &AppState,
) -> Result<Vec<PoolQuestionResponse>, AppError> {
    let ids = questions.iter().flat_map(|question| {
        std::iter::once(question.get_asker().clone()).chain(question.get_approved_by().cloned())
    });
    let people = person_map(ids, &st.db).await?;
    let question_ids: Vec<PoolQuestionId> = questions
        .iter()
        .map(|question| question.get_id().clone())
        .collect();
    let counts = Solution::counts_for(&question_ids, &st.db).await?;
    Ok(questions
        .iter()
        .map(|question| {
            let count = counts.get(question.get_id().key()).copied().unwrap_or(0);
            PoolQuestionResponse::new(question, &people, count)
        })
        .collect())
}

/// Ask a question. Students only — the pool exists for students to get help;
/// staff answer, they don't ask. Born `pending`: invisible to the school
/// until a teacher+ approves it, so nothing unmoderated ever reaches the
/// pool. Attach a photo of the problem afterwards via
/// `POST /questions/{id}/image` (only while pending).
#[utoipa::path(
    post,
    path = "/",
    tag = "questions",
    security(("session_cookie" = [])),
    request_body = AskQuestion,
    responses(
        (status = 201, description = "Question created, pending approval", body = PoolQuestionResponse),
        (status = 400, description = "Invalid title or body", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires the student role", body = ErrorResponse),
    ),
)]
async fn ask_question(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<AskQuestion>,
) -> Result<(StatusCode, Json<PoolQuestionResponse>), AppError> {
    if user.get_role() != Role::Student {
        return Err(AppError::Forbidden("only students ask pool questions"));
    }
    let title = PoolQuestionTitle::try_new(&req.title)?;
    let body = PoolQuestionBody::try_new(&req.body)?;
    let question = PoolQuestion::new(user.get_id(), title, body)
        .insert(&st.db)
        .await?;
    let responses = question_responses(std::slice::from_ref(&question), &st).await?;
    let response = responses
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to render the created question".into()))?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// The question pool, newest first. Everyone (parents excepted) sees every
/// `approved` question; `pending` ones appear only to their asker and to
/// teacher+ — so for a teacher, `?status=pending` is the approval queue.
/// Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/",
    tag = "questions",
    security(("session_cookie" = [])),
    params(PoolQuestionFilter, PageParams),
    responses(
        (status = 200, description = "A page of the questions visible to the caller", body = Page<PoolQuestionResponse>),
        (status = 400, description = "Unknown status, or invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires student role or higher", body = ErrorResponse),
    ),
)]
async fn list_questions(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Query(filter): Query<PoolQuestionFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<PoolQuestionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    if let Some(ref status) = filter.status
        && !POOL_QUESTION_STATUSES.contains(&status.as_str())
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "status",
            reason: "must be one of: pending, approved",
        }));
    }

    let mut questions = if user.get_role().at_least(Role::Teacher) {
        PoolQuestion::list_all(&st.db).await?
    } else {
        PoolQuestion::list_visible_to(user.get_id(), &st.db).await?
    };
    if let Some(ref status) = filter.status {
        questions.retain(|question| question.get_status() == status);
    }

    let total = questions.len() as i64;
    let items = question_responses(paginate(&questions, limit, offset), &st).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// One question. Approved questions are school-wide; a pending one 404s for
/// everyone but its asker and teacher+.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    responses(
        (status = 200, description = "The question", body = PoolQuestionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires student role or higher", body = ErrorResponse),
        (status = 404, description = "No such question (or pending and not yours)", body = ErrorResponse),
    ),
)]
async fn get_question(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
) -> Result<Json<PoolQuestionResponse>, AppError> {
    let question = question_or_404(&st, &id).await?;
    ensure_visible(&question, &user)?;
    let responses = question_responses(std::slice::from_ref(&question), &st).await?;
    let response = responses
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to render the question".into()))?;
    Ok(Json(response))
}

/// Approve a pending question into the school-wide pool (teacher+),
/// stamping the approver. One-way: approved content is frozen, and there is
/// no "rejected" state — to turn a question down, delete it.
#[utoipa::path(
    post,
    path = "/{id}/approve",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    responses(
        (status = 200, description = "The question, now approved", body = PoolQuestionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such question", body = ErrorResponse),
        (status = 409, description = "Already approved", body = ErrorResponse),
    ),
)]
async fn approve_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<PoolQuestionResponse>, AppError> {
    let approved =
        PoolQuestion::approve(&PoolQuestionId::from_key(&id), user.get_id(), &st.db).await?;
    let question = match approved {
        Some(question) => question,
        // Nothing was pending under that id: either it's already approved
        // (409) or it never existed / was deleted (404).
        None => {
            question_or_404(&st, &id).await?;
            return Err(AppError::Conflict("the question is already approved"));
        }
    };
    let responses = question_responses(std::slice::from_ref(&question), &st).await?;
    let response = responses
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to render the approved question".into()))?;
    Ok(Json(response))
}

/// Delete a question — the asker withdrawing their own, or teacher+
/// moderating (this is also how a pending question is rejected). Takes the
/// question's solutions and every image blob — its own and its solutions' —
/// with it.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    responses(
        (status = 204, description = "Deleted, solutions included"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the asker (and not teacher+)", body = ErrorResponse),
        (status = 404, description = "No such question (or pending and not yours)", body = ErrorResponse),
    ),
)]
async fn delete_question(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let question = question_or_404(&st, &id).await?;
    ensure_visible(&question, &user)?;
    if question.get_asker() != user.get_id() && !user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Forbidden(
            "only the asker or a teacher+ may delete a question",
        ));
    }
    let (removed, swept) = PoolQuestion::delete(question.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // Rows went first (in one transaction); now every blob they pointed at —
    // the question's photo and each swept solution's — comes off disk.
    if let Some(file) = removed.get_image_file() {
        remove_blob(&st.files_path, file).await;
    }
    for solution in &swept {
        if let Some(file) = solution.get_image_file() {
            remove_blob(&st.files_path, file).await;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- the question's photo ---------------------------------------------------
// One optional image per question — the snapshot of the problem sheet. Bytes
// on disk under a server-generated ULID, metadata on the question row itself.
// Asker-only, and only while pending: an image landing after approval would
// put unmoderated bytes in the pool, so approval freezes it with the text.

/// Attach (or replace) the question's photo. Asker only, while the question
/// is still `pending` — approval freezes content, image included.
/// `multipart/form-data` with the image under a `file` field; the declared
/// content type must be `image/png`, `image/jpeg`, `image/webp`, or
/// `image/gif` (rasters only — no SVG), the bytes at most the school's
/// `max_file_bytes` (settings).
#[utoipa::path(
    post,
    path = "/{id}/image",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = PoolImageMeta),
        (status = 400, description = "Missing file field, empty file, or a content type outside the image allowlist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the asker", body = ErrorResponse),
        (status = 404, description = "No such question (or pending and not yours)", body = ErrorResponse),
        (status = 409, description = "The question is approved — content is frozen", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_image(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<PoolImageMeta>), AppError> {
    let question = question_or_404(&st, &id).await?;
    ensure_asker_editable(&question, &user)?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;

    // Blob first, row second — a stored row always points at a real blob.
    let file = ulid::Ulid::new().to_string();
    let path = blob_path(&st.files_path, &file);
    tokio::fs::write(&path, &upload.data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the image blob: {err}")))?;
    let size = upload.data.len() as i64;
    match PoolQuestion::set_image(question.get_id(), &file, &content_type, size, &st.db).await {
        // The guarded UPDATE found the question still pending: point-of-truth
        // write done; the replaced blob (if any) comes off disk.
        Ok(Some(before)) => {
            if let Some(replaced) = before.get_image_file() {
                remove_blob(&st.files_path, replaced).await;
            }
            Ok((
                StatusCode::CREATED,
                Json(PoolImageMeta {
                    content_type: content_type.as_str().to_string(),
                    size,
                }),
            ))
        }
        // Approved or deleted mid-upload — the fresh blob is an orphan.
        Ok(None) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(AppError::Conflict("the question is no longer pending"))
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err)
        }
    }
}

/// The question's photo bytes. Access follows the question itself: approved →
/// school-wide, pending → asker and teacher+ only.
#[utoipa::path(
    get,
    path = "/{id}/image",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires student role or higher", body = ErrorResponse),
        (status = 404, description = "No such question or image (or pending and not yours)", body = ErrorResponse),
    ),
)]
async fn get_image(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let question = question_or_404(&st, &id).await?;
    ensure_visible(&question, &user)?;
    match (question.get_image_file(), question.get_image_content_type()) {
        (Some(file), Some(content_type)) => {
            serve_inline_blob(&st.files_path, file, content_type).await
        }
        _ => Err(AppError::NotFound),
    }
}

/// Remove the question's photo. Asker only, while still `pending` — after
/// approval the content (image included) is frozen.
#[utoipa::path(
    delete,
    path = "/{id}/image",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    responses(
        (status = 204, description = "Image removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the asker", body = ErrorResponse),
        (status = 404, description = "No such question or image (or pending and not yours)", body = ErrorResponse),
        (status = 409, description = "The question is approved — content is frozen", body = ErrorResponse),
    ),
)]
async fn delete_image(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let question = question_or_404(&st, &id).await?;
    ensure_asker_editable(&question, &user)?;
    if question.get_image_file().is_none() {
        return Err(AppError::NotFound);
    }
    let before = PoolQuestion::clear_image(question.get_id(), &st.db)
        .await?
        .ok_or(AppError::Conflict("the question is no longer pending"))?;
    if let Some(file) = before.get_image_file() {
        remove_blob(&st.files_path, file).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- solutions --------------------------------------------------------------

/// The solution, provided the caller may see the question it hangs on —
/// visibility never splits below the question (a solution on a question you
/// can't see must not exist for you either).
async fn visible_solution(
    st: &AppState,
    user: &User,
    id: &str,
    sid: &str,
) -> Result<Solution, AppError> {
    let question = question_or_404(st, id).await?;
    ensure_visible(&question, user)?;
    Solution::read_for(&SolutionId::from_key(sid), question.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// The shared front half of the author-only solution writes (body edit,
/// image upload/removal): 404 invisible or missing, then 403 not the author.
/// Teacher+ get no pass here — moderation stays delete-only, a teacher never
/// rewrites someone else's answer. And unlike a question there is no freeze
/// gate: solutions are unmoderated, so their authors edit anytime.
async fn author_solution(
    st: &AppState,
    user: &User,
    id: &str,
    sid: &str,
) -> Result<Solution, AppError> {
    let solution = visible_solution(st, user, id, sid).await?;
    if solution.get_author() != user.get_id() {
        return Err(AppError::Forbidden("only the author may edit a solution"));
    }
    Ok(solution)
}

/// Offer a solution on an approved question. Anyone in the school may —
/// students and staff alike (parents stay read-out). Pending questions take
/// no solutions (409 for those who can see them, 404 for everyone else).
#[utoipa::path(
    post,
    path = "/{id}/solutions",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id")),
    request_body = OfferSolution,
    responses(
        (status = 201, description = "Solution offered", body = SolutionResponse),
        (status = 400, description = "Invalid body", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires student role or higher", body = ErrorResponse),
        (status = 404, description = "No such question (or pending and not yours)", body = ErrorResponse),
        (status = 409, description = "The question is not approved yet", body = ErrorResponse),
    ),
)]
async fn offer_solution(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
    Json(req): Json<OfferSolution>,
) -> Result<(StatusCode, Json<SolutionResponse>), AppError> {
    let question = question_or_404(&st, &id).await?;
    ensure_visible(&question, &user)?;
    if !question.is_approved() {
        return Err(AppError::Conflict(
            "the question is not approved yet — solutions open with the pool",
        ));
    }
    let body = SolutionBody::try_new(&req.body)?;
    let solution = Solution::new(question.get_id(), user.get_id(), body)
        .insert(&st.db)
        .await?;
    let people = person_map([user.get_id().clone()], &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(SolutionResponse::new(&solution, &people)),
    ))
}

/// The question's solutions, oldest first (a discussion reads downward).
/// Visibility follows the question. Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/solutions",
    tag = "questions",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Question id"), PageParams),
    responses(
        (status = 200, description = "A page of the question's solutions", body = Page<SolutionResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires student role or higher", body = ErrorResponse),
        (status = 404, description = "No such question (or pending and not yours)", body = ErrorResponse),
    ),
)]
async fn list_solutions(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SolutionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let question = question_or_404(&st, &id).await?;
    ensure_visible(&question, &user)?;
    let solutions = Solution::list_for(question.get_id(), &st.db).await?;
    let total = solutions.len() as i64;
    let slice = paginate(&solutions, limit, offset);
    let people = person_map(
        slice.iter().map(|solution| solution.get_author().clone()),
        &st.db,
    )
    .await?;
    let items = slice
        .iter()
        .map(|solution| SolutionResponse::new(solution, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Delete a solution — its author withdrawing it, or teacher+ moderating.
/// Takes the solution's image blob with it.
#[utoipa::path(
    delete,
    path = "/{id}/solutions/{sid}",
    tag = "questions",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Question id"),
        ("sid" = String, Path, description = "Solution id"),
    ),
    responses(
        (status = 204, description = "Solution deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the author (and not teacher+)", body = ErrorResponse),
        (status = 404, description = "No such question or solution", body = ErrorResponse),
    ),
)]
async fn delete_solution(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path((id, sid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let solution = visible_solution(&st, &user, &id, &sid).await?;
    if solution.get_author() != user.get_id() && !user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Forbidden(
            "only the author or a teacher+ may delete a solution",
        ));
    }
    // Row first, blob after — a crash in between strands at worst an
    // unreachable file.
    let deleted = solution.delete(&st.db).await?;
    if let Some(file) = deleted.get_image_file() {
        remove_blob(&st.files_path, file).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Edit a solution's body — its author reworking their own answer. Author
/// only, teacher+ included out: moderation stays delete-only (a moderator
/// removes a bad solution, never rewrites someone else's words under their
/// name). No freeze either — solutions are unmoderated, so editing stays
/// open for as long as the solution lives.
#[utoipa::path(
    patch,
    path = "/{id}/solutions/{sid}",
    tag = "questions",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Question id"),
        ("sid" = String, Path, description = "Solution id"),
    ),
    request_body = OfferSolution,
    responses(
        (status = 200, description = "The updated solution", body = SolutionResponse),
        (status = 400, description = "Invalid body", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the author", body = ErrorResponse),
        (status = 404, description = "No such question or solution", body = ErrorResponse),
    ),
)]
async fn edit_solution(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path((id, sid)): Path<(String, String)>,
    Json(req): Json<OfferSolution>,
) -> Result<Json<SolutionResponse>, AppError> {
    let solution = author_solution(&st, &user, &id, &sid).await?;
    let body = SolutionBody::try_new(&req.body)?;
    let updated = Solution::set_body(solution.get_id(), &body, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let people = person_map([user.get_id().clone()], &st.db).await?;
    Ok(Json(SolutionResponse::new(&updated, &people)))
}

// ---- the solution's photo ---------------------------------------------------
// One optional image per solution — the photographed worked steps or diagram.
// Same blob discipline as the question's photo (bytes on disk under a
// server-generated ULID, metadata on the row), but author-only *anytime*:
// solutions carry no moderation state, so there is no approval to freeze
// them.

/// Attach (or replace) the solution's photo. Author only — and at any time,
/// since solutions are never frozen. `multipart/form-data` with the image
/// under a `file` field; the declared content type must be `image/png`,
/// `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the
/// bytes at most the school's `max_file_bytes` (settings).
#[utoipa::path(
    post,
    path = "/{id}/solutions/{sid}/image",
    tag = "questions",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Question id"),
        ("sid" = String, Path, description = "Solution id"),
    ),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = PoolImageMeta),
        (status = 400, description = "Missing file field, empty file, or a content type outside the image allowlist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the author", body = ErrorResponse),
        (status = 404, description = "No such question or solution", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_solution_image(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path((id, sid)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<PoolImageMeta>), AppError> {
    let solution = author_solution(&st, &user, &id, &sid).await?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;

    // Blob first, row second — a stored row always points at a real blob.
    let file = ulid::Ulid::new().to_string();
    let path = blob_path(&st.files_path, &file);
    tokio::fs::write(&path, &upload.data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the image blob: {err}")))?;
    let size = upload.data.len() as i64;
    match Solution::set_image(solution.get_id(), &file, &content_type, size, &st.db).await {
        // Row write done; the replaced blob (if any) comes off disk.
        Ok(Some(before)) => {
            if let Some(replaced) = before.get_image_file() {
                remove_blob(&st.files_path, replaced).await;
            }
            Ok((
                StatusCode::CREATED,
                Json(PoolImageMeta {
                    content_type: content_type.as_str().to_string(),
                    size,
                }),
            ))
        }
        // Deleted mid-upload — the fresh blob is an orphan.
        Ok(None) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(AppError::NotFound)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            Err(err)
        }
    }
}

/// The solution photo's bytes. Access follows the question the solution
/// hangs on — in practice school-wide, since solutions exist only on
/// approved questions.
#[utoipa::path(
    get,
    path = "/{id}/solutions/{sid}/image",
    tag = "questions",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Question id"),
        ("sid" = String, Path, description = "Solution id"),
    ),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires student role or higher", body = ErrorResponse),
        (status = 404, description = "No such question, solution, or image", body = ErrorResponse),
    ),
)]
async fn get_solution_image(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path((id, sid)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let solution = visible_solution(&st, &user, &id, &sid).await?;
    match (solution.get_image_file(), solution.get_image_content_type()) {
        (Some(file), Some(content_type)) => {
            serve_inline_blob(&st.files_path, file, content_type).await
        }
        _ => Err(AppError::NotFound),
    }
}

/// Remove the solution's photo. Author only, anytime — solutions are never
/// frozen.
#[utoipa::path(
    delete,
    path = "/{id}/solutions/{sid}/image",
    tag = "questions",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Question id"),
        ("sid" = String, Path, description = "Solution id"),
    ),
    responses(
        (status = 204, description = "Image removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the author", body = ErrorResponse),
        (status = 404, description = "No such question, solution, or image", body = ErrorResponse),
    ),
)]
async fn delete_solution_image(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Path((id, sid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let solution = author_solution(&st, &user, &id, &sid).await?;
    if solution.get_image_file().is_none() {
        return Err(AppError::NotFound);
    }
    let before = Solution::clear_image(solution.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(file) = before.get_image_file() {
        remove_blob(&st.files_path, file).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
