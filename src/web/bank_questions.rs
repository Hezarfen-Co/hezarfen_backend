//! The question bank: a library of reusable question templates. A template is
//! born `private` — visible to its owner (and admins) alone — and becomes
//! school-wide only when its owner PATCHes `visibility` to `school`, because a
//! template carries `correct`, the answer key, and saving a live exam's
//! question to the bank must not broadcast it. Teacher+ read and instantiate
//! every template they can see (`POST /exams/{id}/questions/from-bank/{bid}`,
//! in `web/exams`); a template they cannot see is a 404 on every route, never a
//! 403, since a 403 would confirm it exists. Only the owner (admins aside) may
//! edit or delete one. A template carries the same content an exam question does — text,
//! points, a `choice`/`text` spec, an optional illustration, and per-option
//! pictures — minus any exam tie: its `subject` is origin metadata only (the
//! same-course rule is checked at instantiate time against the target exam's
//! course, never here, since the bank spans courses). Instantiate and
//! save-to-bank both *copy* rows and blobs; the two sides never share one.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::domain::bank_question::{BankQuestion, BankQuestionId, BankVisibility};
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::exam_question::{
    Choice, ChoiceId, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::note_file::FileContentType;
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::subject::{Subject, SubjectId};
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::dto::person_map;
use super::subjects::subject_must_exist;
use super::exams::ChoiceResponse;
use super::{
    ChoiceBody, Page, PageParams, RequireTeacher, UploadFileForm, blob_path, image_content_type,
    read_upload, remove_blob, serve_inline_blob, set_or_clear,
};

/// Serializes bank writes that adopt a `subject` against a concurrent subject
/// delete. Bank create/update take the reader lease across their
/// subject-exists check and the write; the subject delete takes the writer
/// lease across the cascade that clears `subject` off every template, so a
/// template can't land on a subject that vanished mid-flight (it would outlive
/// the sweep) — the bank twin of the
/// [`super::exams::EXAM_LOCK`]/[`super::homework::HOMEWORK_LOCK`] guards.
/// Save-to-bank (`question_to_bank`) takes the same reader lease across its
/// question read and insert: the source question can be re-tagged/deleted, so
/// its subject isn't otherwise pinned. Held after EXAM_LOCK/HOMEWORK_LOCK in
/// the delete path; bank writers take only this one, so no cycle.
pub(crate) static BANK_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_question, list_questions))
        .routes(routes!(get_question, update_question, delete_question))
        // The image routes get their own HTTP body cap, like the exam and
        // note-file ones: the server-wide hard ceiling plus multipart framing.
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
struct CreateBankQuestion {
    /// The subject this template came from — origin metadata only. Any of the
    /// school's subjects; it is *not* held to a course here (the bank spans
    /// courses), only checked against the target exam's course when the
    /// template is instantiated.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    subject_id: String,
    #[schema(example = "What is 2 + 2?")]
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    /// This template's default point value, `1`–`100`.
    #[schema(example = 10)]
    points: i64,
    /// The options of a `choice` question (2–10 of them); omit for `text`.
    /// Each carries an `id` naming it within this payload — the server mints
    /// the stored ids and returns them.
    choices: Option<Vec<ChoiceBody>>,
    /// The `id` of the right option, as sent in `choices`; required for
    /// `choice`, absent for `text`.
    #[schema(example = "b")]
    correct: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateBankQuestion {
    /// Re-tag the template's origin subject. Omit to keep the current one
    /// (which is `null` if that subject has since been deleted).
    subject_id: Option<String>,
    text: Option<String>,
    points: Option<i64>,
    /// `choice` or `text`. Switching kinds needs the other fields to follow:
    /// send `choices` + `correct` when moving to `choice`, explicit `null`s
    /// when moving to `text`.
    kind: Option<String>,
    /// Omit to keep the stored options; send `null` to drop them (text
    /// questions only). Send each option back with the `id` it was returned
    /// with to keep it — its picture rides along. Only options whose id is
    /// absent from the new list lose their picture.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<Vec<ChoiceBody>>)]
    choices: Option<Option<Vec<ChoiceBody>>>,
    /// The `id` of the right option. Omit to keep; `null` to clear (text
    /// questions only).
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    correct: Option<Option<String>>,
    /// `private` (owner + admins only) or `school` (every teacher). Publishing
    /// hands the template's `correct` answer key — and its pictures — to every
    /// teacher in the school, so it is only ever an explicit choice.
    #[schema(example = "school")]
    visibility: Option<String>,
}

/// Filters for the bank list.
#[derive(Deserialize, IntoParams)]
struct BankQuestionFilter {
    /// Narrow to templates whose origin `subject` matches this id. Omit for
    /// the whole bank.
    subject: Option<String>,
    /// Narrow to templates owned by this user (a user id, or `me` for the
    /// caller). Omit for every owner's templates.
    owner: Option<String>,
    /// Case-insensitive fragment of the question text. Blank or omitted
    /// matches every template.
    #[param(example = "photosynthesis")]
    q: Option<String>,
    /// `private` or `school`. Narrows what the caller can already see, never
    /// widens it: `private` is effectively "my drafts" (nobody else's private
    /// template is ever visible), `school` is the published library. Omit for
    /// both.
    #[param(example = "school")]
    visibility: Option<String>,
}

/// A bank template as its readers see it — `correct` included (teacher+ only,
/// so the answer key is theirs to see). Its images, if any, are fetched
/// through the image endpoints.
#[derive(Serialize, ToSchema)]
pub(crate) struct BankQuestionResponse {
    id: String,
    /// The teacher who owns the template (only they, or an admin, may edit it).
    owner: String,
    /// The origin subject (metadata only — not enforced against a course).
    /// `null` once that subject was deleted: deleting a subject clears the
    /// bank's copy of it rather than being blocked by it.
    subject: Option<String>,
    text: String,
    /// `choice` or `text`.
    #[schema(example = "choice")]
    kind: String,
    points: i64,
    /// The options with their stable ids (`choice` templates only).
    choices: Option<Vec<ChoiceResponse>>,
    /// The id of the right option (`choice` templates only).
    correct: Option<String>,
    /// The template's illustration, if one was uploaded (any kind).
    image: Option<BankImageMeta>,
    /// Per-option pictures, aligned with `choices` (`choice` templates only).
    choice_images: Option<Vec<Option<BankImageMeta>>>,
    /// The exam this template was saved off, if any (a direct-authored
    /// template has none).
    source_exam: Option<String>,
    /// `private` (owner + admins only) or `school` (every teacher). New
    /// templates start `private`.
    #[schema(example = "private")]
    visibility: String,
    /// When the template was saved, UTC unix-milliseconds.
    created_at: i64,
    /// The origin subject's name, resolved for display. Empty when the subject
    /// is gone (deleted, so `subject` is `null` too) — and on the
    /// single-template endpoints, which don't join.
    #[schema(example = "Limits and continuity")]
    subject_name: String,
    /// The owner's display name (full name, else username). Empty when the user
    /// is gone — and on the single-template endpoints, which don't join.
    #[schema(example = "Ada Lovelace")]
    owner_name: String,
    /// How many exam questions were created from this template — the
    /// divergence surface, since each of them is a detached copy that a later
    /// edit here does *not* reach. `0` for an unused template, and `0` on the
    /// single-template endpoints, which don't join (list-only, exactly like
    /// `subject_name`/`owner_name`).
    #[schema(example = 3)]
    used_count: i64,
}

impl BankQuestionResponse {
    /// The names and the usage tally are joined on by the list endpoint alone
    /// (see [`Self::with_names`]); every other endpoint returns one template
    /// and leaves them empty/zero.
    pub(crate) fn with_names(
        question: &BankQuestion,
        images: &[BankQuestionImage],
        subject_name: String,
        owner_name: String,
        used_count: i64,
    ) -> Self {
        Self {
            subject_name,
            owner_name,
            used_count,
            ..Self::new(question, images)
        }
    }

    pub(crate) fn new(question: &BankQuestion, images: &[BankQuestionImage]) -> Self {
        Self {
            subject_name: String::new(),
            owner_name: String::new(),
            used_count: 0,
            id: question.get_id().key().to_string(),
            owner: question.get_owner().key().to_string(),
            subject: question.get_subject().map(|s| s.key().to_string()),
            text: question.get_text().as_str().to_string(),
            kind: question.get_kind().as_str().to_string(),
            points: question.get_points().as_i64(),
            choices: ChoiceResponse::list(question.get_choices()),
            correct: question.get_correct().map(|id| id.as_str().to_string()),
            image: bank_image_meta(images, None),
            choice_images: bank_choice_image_metas(question, images),
            source_exam: question.get_source_exam().map(|e| e.key().to_string()),
            visibility: question.get_visibility().as_str().to_string(),
            created_at: question.get_created_at().as_millis(),
        }
    }
}

/// The template's option named by `choice_id` — a 400 for a text template or an
/// id the template doesn't have, mirroring `exams::choice_slot`.
fn bank_choice_slot(question: &BankQuestion, choice_id: &str) -> Result<ChoiceId, AppError> {
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

/// The template's slot out of its image rows.
fn bank_image_meta(images: &[BankQuestionImage], slot: Option<&ChoiceId>) -> Option<BankImageMeta> {
    images
        .iter()
        .find(|image| image.get_slot() == slot)
        .map(BankImageMeta::new)
}

/// The per-choice metas, aligned position-for-position with `choices` (`None`
/// entries = that option has no picture); `None` whole for text templates.
/// A response-only projection rebuilt from the current list — the pictures
/// themselves are stored against choice ids.
fn bank_choice_image_metas(
    question: &BankQuestion,
    images: &[BankQuestionImage],
) -> Option<Vec<Option<BankImageMeta>>> {
    question.get_choices().map(|choices| {
        choices
            .iter()
            .map(|choice| bank_image_meta(images, Some(choice.get_id())))
            .collect()
    })
}

/// The image rows of `questions` bucketed by template key — one query feeding
/// a whole page (neither an image query per row, nor the school's every image).
async fn bank_images_by_question(
    questions: &[&BankQuestionId],
    db: &crate::database::Database,
) -> Result<HashMap<String, Vec<BankQuestionImage>>, AppError> {
    let mut buckets: HashMap<String, Vec<BankQuestionImage>> = HashMap::new();
    for image in BankQuestionImage::list_for_questions(questions, db).await? {
        buckets
            .entry(image.get_bank_question().key().to_string())
            .or_default()
            .push(image);
    }
    Ok(buckets)
}

/// A stored bank image's metadata; the bytes come from the image endpoints.
#[derive(Serialize, ToSchema)]
struct BankImageMeta {
    #[schema(example = "image/png")]
    content_type: String,
    #[schema(example = 24_576)]
    size: i64,
}

impl BankImageMeta {
    fn new(image: &BankQuestionImage) -> Self {
        Self {
            content_type: image.get_content_type().as_str().to_string(),
            size: image.get_size(),
        }
    }
}

/// The template, or a 404.
async fn question_or_404(st: &AppState, bid: &str) -> Result<BankQuestion, AppError> {
    BankQuestion::read(&BankQuestionId::from_key(bid), &st.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Whether `user` may read the template at all: it is published to the school,
/// or it is theirs, or they are an admin.
///
/// Admins **do** see `private` templates. They already read every exam's
/// questions — `correct` included — through `can_manage_course`, and
/// `ensure_owner` already lets them edit and delete any template; letting them
/// delete a row they may not look at would be the odd rule, not this one.
pub(crate) fn can_see(question: &BankQuestion, user: &User) -> bool {
    question.get_visibility().is_school()
        || question.get_owner() == user.get_id()
        || user.get_role().at_least(Role::Admin)
}

/// Reads are gated by [`can_see`], and a mutation additionally needs ownership:
/// the caller owns the template, or is an admin. Everyone else gets a 403 —
/// but only for a template they can see; an invisible one is a 404 long before
/// this runs, so a 403 never doubles as proof the template exists.
fn ensure_owner(question: &BankQuestion, user: &User) -> Result<(), AppError> {
    if question.get_owner() == user.get_id() || user.get_role().at_least(Role::Admin) {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "only the template's owner or an admin can change it",
    ))
}

/// The template, provided the caller may see it — the shared front half of
/// every read. A template the caller may not see is a **404, not a 403**: a
/// 403 would confirm that someone else's template exists under that id.
pub(crate) async fn visible_question(
    st: &AppState,
    user: &User,
    bid: &str,
) -> Result<BankQuestion, AppError> {
    let question = question_or_404(st, bid).await?;
    if !can_see(&question, user) {
        return Err(AppError::NotFound);
    }
    Ok(question)
}

/// The template, provided the caller may edit it — the shared front half of
/// every owner-gated write (PATCH, delete, image writes).
async fn owned_question(st: &AppState, user: &User, bid: &str) -> Result<BankQuestion, AppError> {
    let question = visible_question(st, user, bid).await?;
    ensure_owner(&question, user)?;
    Ok(question)
}

/// The whole bank-image write tail: new blob to disk, row UPSERT (the
/// deterministic per-slot id makes it a replace), then the replaced blob off
/// disk. A failed row write takes the fresh blob back out.
pub(crate) async fn store_image(
    st: &AppState,
    question: &BankQuestionId,
    slot: Option<&ChoiceId>,
    content_type: FileContentType,
    data: &[u8],
) -> Result<BankQuestionImage, AppError> {
    let replaced = BankQuestionImage::read_slot(question, slot, &st.db).await?;
    let image = BankQuestionImage::new(question, slot, content_type, data.len() as i64);
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

/// Add a template to the bank. Requires teacher+. `subject_id` is origin
/// metadata (any subject — the same-course rule lives at instantiate time),
/// so it need only exist (an unknown subject is a `400`). `choice` templates
/// carry 2–10 `choices` plus the `correct` index; `text` templates carry
/// neither. The caller becomes the owner.
#[utoipa::path(
    post,
    path = "/",
    tag = "bank",
    security(("session_cookie" = [])),
    request_body = CreateBankQuestion,
    responses(
        (status = 201, description = "Template created", body = BankQuestionResponse),
        (status = 400, description = "Invalid text, kind, points, choices, or correct — or an unknown subject", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn create_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateBankQuestion>,
) -> Result<(StatusCode, Json<BankQuestionResponse>), AppError> {
    // Reader lease of [`BANK_LOCK`]: the exists-check and the insert are one
    // unit, so a subject delete can't slip between them (see the lock's doc).
    let _guard = BANK_LOCK.read().await;
    // Origin metadata only — no course to check it against, but it must exist.
    let subject = subject_must_exist(&req.subject_id, &st.db).await?;
    let text = QuestionText::try_new(&req.text)?;
    let points = QuestionPoints::try_new(req.points)?;
    // Nothing stored to match against on create: every option is new and every
    // id is minted here.
    let spec = QuestionSpec::try_new(
        QuestionKind::try_new(&req.kind)?,
        ChoiceBody::into_inputs(req.choices),
        req.correct,
        &[],
    )?;
    let question =
        BankQuestion::create(user.get_id().clone(), subject, text, points, spec, &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(BankQuestionResponse::new(&question, &[])),
    ))
}

/// The bank the caller may see — their own templates plus the ones published
/// to the school (admins see every one), **newest first**. `?subject=` narrows to one origin subject; `?owner=` to one owner
/// (a user id, or `me` for the caller); `?q=` to a case-insensitive fragment of
/// the question text; `?visibility=private|school` to one shelf — it narrows
/// what the caller may already see and never widens it, so `private` is "my
/// drafts" and `school` the published library. Paged via `?limit=&offset=` (omit `limit` for all of
/// them); returns a `{items, total, limit, offset}` envelope, where `total`
/// counts every match under the same filters, not just this page. Each item
/// carries the resolved `subject_name`/`owner_name` so a client needn't look
/// them up per row, plus `used_count` — how many exam questions were copied out
/// of that template (one grouped query for the page, not one per row).
#[utoipa::path(
    get,
    path = "/",
    tag = "bank",
    security(("session_cookie" = [])),
    params(BankQuestionFilter, PageParams),
    responses(
        (status = 200, description = "A page of the bank's templates (all of them when unpaged)", body = Page<BankQuestionResponse>),
        (status = 400, description = "Invalid limit, offset, or visibility", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn list_questions(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Query(filter): Query<BankQuestionFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<BankQuestionResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let subject = filter.subject.as_deref().map(SubjectId::from_key);
    let owner = filter.owner.as_deref().map(|owner| match owner {
        "me" => user.get_id().clone(),
        id => UserId::from_key(id),
    });
    // Same newtype the PATCH validates against, so an unknown shelf is the same
    // 400 ("visibility must be private or school") on both routes.
    let visibility = filter
        .visibility
        .as_deref()
        .map(BankVisibility::try_new)
        .transpose()?;
    // The visibility gate is one of those SQL filters, never a post-filter over
    // the page: `total` counts what the caller may see, so paging can't hand
    // back short pages full of holes where someone else's private templates sat.
    let viewer = (!user.get_role().at_least(Role::Admin)).then(|| user.get_id().clone());
    // Filters, order, and window are all SQL — `total` comes from a count over
    // the same WHERE, so a client can page past the first window.
    let (questions, total) = BankQuestion::list(
        viewer.as_ref(),
        owner.as_ref(),
        subject.as_ref(),
        visibility.as_ref(),
        filter.q.as_deref(),
        limit,
        offset,
        &st.db,
    )
    .await?;

    // Four bulk joins over the page alone: its images, its subjects' names,
    // its owners' names, and how many exam questions each template spawned.
    // A missing row renders empty, never fails the list.
    let ids: Vec<&BankQuestionId> = questions.iter().map(BankQuestion::get_id).collect();
    let buckets = bank_images_by_question(&ids, &st.db).await?;
    // One grouped query for the whole page — never a count per row.
    let used = BankQuestion::usage_counts(&ids, &st.db).await?;
    let subject_ids: Vec<&SubjectId> = questions.iter().filter_map(BankQuestion::get_subject).collect();
    let subject_names: HashMap<String, String> = Subject::list_by_ids(&subject_ids, &st.db)
        .await?
        .iter()
        .map(|subject| {
            (
                subject.get_id().key().to_string(),
                subject.get_name().as_str().to_string(),
            )
        })
        .collect();
    let people = person_map(questions.iter().map(|q| q.get_owner().clone()), &st.db).await?;

    let empty: Vec<BankQuestionImage> = Vec::new();
    let items = questions
        .iter()
        .map(|question| {
            let images = buckets.get(question.get_id().key()).unwrap_or(&empty);
            let subject_name = question
                .get_subject()
                .and_then(|subject| subject_names.get(subject.key()).cloned())
                .unwrap_or_default();
            let owner_name = people
                .get(question.get_owner().key())
                .map(|person| {
                    person
                        .display_name
                        .clone()
                        .unwrap_or_else(|| person.username.clone())
                })
                .unwrap_or_default();
            let used_count = used.get(question.get_id().key()).copied().unwrap_or(0);
            BankQuestionResponse::with_names(question, images, subject_name, owner_name, used_count)
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// One template by id. Visible ones only: a `private` template belonging to
/// someone else is a 404, not a 403 — a 403 would confirm it exists.
#[utoipa::path(
    get,
    path = "/{bid}",
    tag = "bank",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Bank question id")),
    responses(
        (status = 200, description = "The template", body = BankQuestionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such template, or one the caller may not see", body = ErrorResponse),
    ),
)]
async fn get_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(bid): Path<String>,
) -> Result<Json<BankQuestionResponse>, AppError> {
    let question = visible_question(&st, &user, &bid).await?;
    let images = BankQuestionImage::list_for_question(question.get_id(), &st.db).await?;
    Ok(Json(BankQuestionResponse::new(&question, &images)))
}

/// Edit a template. Owner only (admins aside — 403 otherwise). Omitted fields
/// keep their value; `kind`/`choices`/`correct` are re-validated as a unit, so
/// a kind switch must bring the matching fields along. Replacing or clearing
/// `choices` drops the old options' pictures. Bank templates never freeze —
/// they have no exam tie.
///
/// This is also the publish switch: `visibility: "school"` shares the template
/// with every teacher, `"private"` pulls it back. By design an **admin can
/// publish (or unpublish) another teacher's private template** — the ownership
/// gate here is the same admin-bypassing one that lets an admin edit or delete
/// any template, and it is not narrowed for this field.
#[utoipa::path(
    patch,
    path = "/{bid}",
    tag = "bank",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Bank question id")),
    request_body = UpdateBankQuestion,
    responses(
        (status = 200, description = "Updated template", body = BankQuestionResponse),
        (status = 400, description = "Invalid text, kind, points, choices, or correct — or an unknown subject", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the template's owner (and not an admin)", body = ErrorResponse),
        (status = 404, description = "No such template", body = ErrorResponse),
    ),
)]
async fn update_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(bid): Path<String>,
    Json(req): Json<UpdateBankQuestion>,
) -> Result<Json<BankQuestionResponse>, AppError> {
    let question = owned_question(&st, &user, &bid).await?;

    // Reader lease of [`BANK_LOCK`] across the exists-check and the update, so a
    // re-tag to a subject that's being deleted can't straddle the delete-guard.
    let _guard = BANK_LOCK.read().await;
    // Omitted keeps the stored subject — which may already be `None`, cleared
    // by that subject's delete.
    let subject = match req.subject_id {
        Some(ref subject_id) => Some(subject_must_exist(subject_id, &st.db).await?),
        None => question.get_subject().cloned(),
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
    let visibility = match req.visibility {
        Some(ref visibility) => BankVisibility::try_new(visibility)?,
        None => question.get_visibility().clone(),
    };

    let bid = question.get_id().clone();
    let updated = question
        .update(subject, text, points, spec, visibility, &st.db)
        .await?;
    // Only the options that are actually *gone* lose their pictures — an option
    // that survives the edit keeps its image wherever it moved in the list.
    let keep: Vec<ChoiceId> = updated
        .get_choices()
        .unwrap_or_default()
        .iter()
        .map(|choice| choice.get_id().clone())
        .collect();
    for image in BankQuestionImage::delete_choices_not_in(&bid, &keep, &st.db).await? {
        remove_blob(&st.files_path, image.get_file()).await;
    }
    let images = BankQuestionImage::list_for_question(updated.get_id(), &st.db).await?;
    Ok(Json(BankQuestionResponse::new(&updated, &images)))
}

/// Delete a template. Owner only (admins aside). Cascades its image rows and
/// takes their blobs off disk.
#[utoipa::path(
    delete,
    path = "/{bid}",
    tag = "bank",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Bank question id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the template's owner (and not an admin)", body = ErrorResponse),
        (status = 404, description = "No such template", body = ErrorResponse),
    ),
)]
async fn delete_question(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(bid): Path<String>,
) -> Result<StatusCode, AppError> {
    let question = owned_question(&st, &user, &bid).await?;
    // Rows go first (the delete cascades the image rows), blobs after — a crash
    // in between strands at worst an unreachable blob.
    let images = BankQuestionImage::list_for_question(question.get_id(), &st.db).await?;
    question.delete(&st.db).await?;
    for image in &images {
        remove_blob(&st.files_path, image.get_file()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- template images --------------------------------------------------------
// A template may carry one illustration (any kind) and, on choice templates,
// one picture per option — mirroring exam question images, minus the exam tie
// and the freeze. Mutations are owner-gated; reads follow the template's
// visibility (a template the caller can't see 404s, images included).

/// Attach (or replace) a template's illustration. Owner only (admins aside).
/// `multipart/form-data` with the image under a `file` field; the declared
/// content type must be `image/png`, `image/jpeg`, `image/webp`, or
/// `image/gif` (rasters only — no SVG), the bytes at most the school's
/// `max_file_bytes` (settings).
#[utoipa::path(
    post,
    path = "/{bid}/image",
    tag = "bank",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Bank question id")),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = BankImageMeta),
        (status = 400, description = "Missing file field, empty file, or a content type outside the image allowlist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the template's owner (and not an admin)", body = ErrorResponse),
        (status = 404, description = "No such template", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(bid): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<BankImageMeta>), AppError> {
    let question = owned_question(&st, &user, &bid).await?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    let stored = store_image(&st, question.get_id(), None, content_type, &upload.data).await?;
    Ok((StatusCode::CREATED, Json(BankImageMeta::new(&stored))))
}

/// A template's illustration bytes. Readable by anyone who may see the
/// template — 404 otherwise, never a 403.
#[utoipa::path(
    get,
    path = "/{bid}/image",
    tag = "bank",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Bank question id")),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such template or image", body = ErrorResponse),
    ),
)]
async fn get_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(bid): Path<String>,
) -> Result<Response, AppError> {
    let question = visible_question(&st, &user, &bid).await?;
    let image = BankQuestionImage::read_slot(question.get_id(), None, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}

/// Remove a template's illustration. Owner only (admins aside).
#[utoipa::path(
    delete,
    path = "/{bid}/image",
    tag = "bank",
    security(("session_cookie" = [])),
    params(("bid" = String, Path, description = "Bank question id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the template's owner (and not an admin)", body = ErrorResponse),
        (status = 404, description = "No such template or image", body = ErrorResponse),
    ),
)]
async fn delete_question_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(bid): Path<String>,
) -> Result<StatusCode, AppError> {
    let question = owned_question(&st, &user, &bid).await?;
    let image = BankQuestionImage::read_slot(question.get_id(), None, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let image = image.delete(&st.db).await?;
    remove_blob(&st.files_path, image.get_file()).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Attach (or replace) one option's picture on a `choice` template. Owner only
/// (admins aside). Same form, limits, and rules as the illustration upload;
/// `index` is the option's zero-based position. Replacing the `choices` list
/// drops all its option pictures.
#[utoipa::path(
    post,
    path = "/{bid}/choices/{choice_id}/image",
    tag = "bank",
    security(("session_cookie" = [])),
    params(
        ("bid" = String, Path, description = "Bank question id"),
        ("choice_id" = String, Path, description = "Choice id, as returned in the template's `choices`"),
    ),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "Image stored", body = BankImageMeta),
        (status = 400, description = "Missing file field, empty file, a content type outside the image allowlist, a text template, or an unknown choice id", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the template's owner (and not an admin)", body = ErrorResponse),
        (status = 404, description = "No such template", body = ErrorResponse),
        (status = 413, description = "Image exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((bid, choice_id)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<BankImageMeta>), AppError> {
    let question = owned_question(&st, &user, &bid).await?;
    let slot = bank_choice_slot(&question, &choice_id)?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    let upload = read_upload(&mut multipart, limit).await?;
    let content_type = image_content_type(&upload.content_type.unwrap_or_default())?;
    let stored = store_image(
        &st,
        question.get_id(),
        Some(&slot),
        content_type,
        &upload.data,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(BankImageMeta::new(&stored))))
}

/// One option's picture bytes. Readable by anyone who may see the template —
/// 404 otherwise, never a 403.
#[utoipa::path(
    get,
    path = "/{bid}/choices/{choice_id}/image",
    tag = "bank",
    security(("session_cookie" = [])),
    params(
        ("bid" = String, Path, description = "Bank question id"),
        ("choice_id" = String, Path, description = "Choice id, as returned in the template's `choices`"),
    ),
    responses(
        (status = 200, description = "The image bytes", content_type = "image/*"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "No such template or image", body = ErrorResponse),
    ),
)]
async fn get_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((bid, choice_id)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let question = visible_question(&st, &user, &bid).await?;
    let slot = bank_choice_slot(&question, &choice_id)?;
    let image = BankQuestionImage::read_slot(question.get_id(), Some(&slot), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    serve_inline_blob(&st.files_path, image.get_file(), image.get_content_type()).await
}

/// Remove one option's picture. Owner only (admins aside).
#[utoipa::path(
    delete,
    path = "/{bid}/choices/{choice_id}/image",
    tag = "bank",
    security(("session_cookie" = [])),
    params(
        ("bid" = String, Path, description = "Bank question id"),
        ("choice_id" = String, Path, description = "Choice id, as returned in the template's `choices`"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the template's owner (and not an admin)", body = ErrorResponse),
        (status = 404, description = "No such template or image", body = ErrorResponse),
    ),
)]
async fn delete_choice_image(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((bid, choice_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let question = owned_question(&st, &user, &bid).await?;
    let slot = bank_choice_slot(&question, &choice_id)?;
    let image = BankQuestionImage::read_slot(question.get_id(), Some(&slot), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let image = image.delete(&st.db).await?;
    remove_blob(&st.files_path, image.get_file()).await;
    Ok(StatusCode::NO_CONTENT)
}
