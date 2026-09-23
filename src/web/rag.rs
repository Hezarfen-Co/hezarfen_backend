//! The RAG nest: browser ⇄ backend ⇄ AI service (over the QUIC bridge),
//! scoped to a school's course-note corpus.
//!
//! The relay contract is [`super::chatbot`]'s, shape for shape: the backend
//! owns everything except the answer — auth, the per-user rate limit, the
//! thread, the payload format, and the last look at the reply before it is
//! stored. There are no intent or rule tables, and every thread is private to
//! its owner. What differs is the **scope**: every question carries the
//! `(class, course)` pairs the corpus is routed by, derived server-side
//! ([`crate::service::rag_scope`]: a manager or an admin gets every class-course
//! pair in the school, everyone else their own memberships) and never from the
//! request body, and every citation the service returns is resolved here to
//! the course-note file that owns the cited document — the `doc_id` only the
//! backend can turn into something a reader can open.
//!
//! The school's existing AI caps govern both nests: a RAG thread is reserved
//! against the same `max_chatbot_threads` seat count, a prompt and its answer
//! are bounded by the same `max_chatbot_message_len`, and the history replayed
//! to the service is the same `chatbot_history_turns` window. Only the
//! per-minute tier is RAG's own (`DEFAULT_RAG_RATE_LIMIT`), because every
//! message here spends a retrieval over the whole corpus before it generates
//! anything.
//!
//! Sending is asynchronous by design. `POST .../messages` writes the user's
//! turn plus an empty `pending` assistant row and answers `202` immediately;
//! a retrieval-then-answer round trip can outlast the request-timeout layer
//! that guards every HTTP handler, so nothing is awaited on the request path.
//! The client then either polls `GET .../messages/{mid}` or opens its SSE
//! stream — both read the same row, so they can never disagree.
//!
//! The bridge round trip runs in a `tokio::spawn` the POST leaves behind. A
//! restart therefore orphans a turn in flight: its row stays `pending` until a
//! reader projects it failed, and the boot sweep stamps that verdict durably
//! ([`crate::constant::RAG_PENDING_STALE_SECS`]).
//!
//! The two **study** doors (`POST /rag/summarize`, `POST /rag/questions`) are
//! the exception to that shape, and deliberately: a summary or a question set
//! is one artifact about one range of one corpus, so there is no thread to
//! open, no row to poll and nothing stored — the request waits for the
//! artifact, under a dispatch deadline kept inside the middleware's own
//! envelope.

use std::time::Duration;

use crate::tenant::ResolvedTenant;
use crate::web::tenant_state::{SchoolIdCookie, State, TenantExt};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::types::Json as SqlJson;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::chat::ChatRole;
use crate::ai::rag_chat::RagScopePair;
use crate::ai::rag_study::{
    self, RagQuestion, RagQuestionsPayload, RagQuestionsReply, RagScope, RagSummarizePayload,
    RagSummarizeReply, RagSummaryCitation,
};
use crate::ai::AiBridge;
use crate::constant::{
    AI_RAG_CHAT_CAPABILITY, AI_RAG_QUESTIONS_CAPABILITY, AI_RAG_SUMMARIZE_CAPABILITY,
    CHAT_STREAM_POLL_MS, MAX_CHATBOT_MESSAGE_LEN, MAX_RAG_QUESTIONS, REPLY_CHUNKS,
};
use crate::database::Database;
use crate::domain::rag_message::{
    RagCitedFile, RagContent, RagMessage, RagMessageId, RagMessageRole, RagMessageStatus,
};
use crate::domain::rag_thread::{RagThread, RagThreadId, RagThreadTitle};
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::user::UserId;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::{AppState, scoped_key};

use super::{CurrentUser, Page, PageParams, ai_unavailable, slice_reply, sse_error, sse_send};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_thread, list_threads))
        .routes(routes!(rename_thread, delete_thread))
        .routes(routes!(list_messages, send_message))
        .routes(routes!(read_message))
        .routes(routes!(stream_message))
        // One `routes!(…)` call is one *path*: two handlers in it must share
        // it (different methods). These two doors share neither.
        .routes(routes!(summarize))
        .routes(routes!(questions))
}

// ---- threads ----------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct CreateRagThread {
    /// Optional thread name, up to 200 characters. Blank counts as absent —
    /// an untitled thread is normal (the UI labels it from its first turn).
    #[schema(max_length = 200, example = "Fizik ödevi")]
    title: Option<String>,
}

/// One RAG thread. `updated_at` moves on every turn, and the list is sorted by
/// it, so the thread just written to is always first.
#[derive(Serialize, ToSchema)]
struct RagThreadResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    #[schema(example = "Fizik ödevi")]
    title: Option<String>,
    /// UTC unix-milliseconds, server-stamped.
    created_at: i64,
    /// UTC unix-milliseconds of the last turn, server-stamped.
    updated_at: i64,
}

impl RagThreadResponse {
    fn new(thread: &RagThread) -> Self {
        Self {
            id: thread.get_id().key().to_string(),
            title: thread.get_title().map(|title| title.as_str().to_string()),
            created_at: thread.get_created_at().as_millis(),
            updated_at: thread.get_updated_at().as_millis(),
        }
    }
}

/// Start a new RAG thread, optionally named. Every authenticated role may ask
/// — parents included. A user may keep up to the school's
/// `max_chatbot_threads` threads *across both AI nests*; at the cap the
/// request is refused (409) until an old thread is deleted — the cap is
/// storage protection, not a usage quota (that is the per-minute message
/// limit).
#[utoipa::path(
    post,
    path = "/threads",
    tag = "rag",
    security(("session_cookie" = [])),
    request_body = CreateRagThread,
    responses(
        (status = 201, description = "The new thread", body = RagThreadResponse),
        (status = 400, description = "Invalid title", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 409, description = "At the school's thread cap", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_thread(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<CreateRagThread>,
) -> Result<(StatusCode, Json<RagThreadResponse>), AppError> {
    // Blank is "untitled", not an error — the same rule the chatbot nest
    // applies to a thread name.
    let title = match req.title.as_deref().map(str::trim).unwrap_or_default() {
        "" => None,
        value => Some(RagThreadTitle::try_new(value)?),
    };

    // The cap is checked and the row written as one critical section in the
    // domain: counting here and creating after would over-admit under
    // concurrency (the database does not serialize a count against inserts).
    let thread = service::rag_thread::create_capped(&st.db, user.get_id(), title).await?;
    Ok((
        StatusCode::CREATED,
        Json(RagThreadResponse::new(&thread)),
    ))
}

/// The caller's own threads, most recently active first. Paged via
/// `?limit=&offset=`. Nobody — no teacher, no admin — reads anyone else's.
#[utoipa::path(
    get,
    path = "/threads",
    tag = "rag",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's threads", body = Page<RagThreadResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_threads(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<RagThreadResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (threads, total) =
        service::rag_thread::list_for_user(&st.db, user.get_id(), limit, offset).await?;
    let items = threads.iter().map(RagThreadResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[derive(Deserialize, ToSchema)]
struct RenameRagThread {
    /// The new name, up to 200 characters. `null` — or blank — clears it back
    /// to untitled, the same rule the create route applies.
    #[schema(max_length = 200, example = "Fizik ödevi")]
    title: Option<String>,
}

/// Rename a thread, or clear its name (`title: null`). Owner only; someone
/// else's thread is a `404`, never a `403`. The edit counts as activity, so the
/// thread moves to the top of the list.
///
/// Nothing names a thread automatically: the first message is not turned into
/// a title, and the AI service is never asked for one.
#[utoipa::path(
    patch,
    path = "/threads/{id}",
    tag = "rag",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "RagThread id")),
    request_body = RenameRagThread,
    responses(
        (status = 200, description = "The renamed thread", body = RagThreadResponse),
        (status = 400, description = "Invalid title", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn rename_thread(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<RenameRagThread>,
) -> Result<Json<RagThreadResponse>, AppError> {
    let title = match req.title.as_deref().map(str::trim).unwrap_or_default() {
        "" => None,
        value => Some(RagThreadTitle::try_new(value)?),
    };
    let thread = own_thread(&id, user.get_id(), &st.db).await?;
    let renamed = service::rag_thread::rename(&st.db, &thread, title).await?;
    Ok(Json(RagThreadResponse::new(&renamed)))
}

/// Delete a thread and every turn in it, permanently. Owner only; someone
/// else's thread is a `404`, never a `403` (its existence is not leaked).
#[utoipa::path(
    delete,
    path = "/threads/{id}",
    tag = "rag",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "RagThread id")),
    responses(
        (status = 204, description = "Deleted, with its messages"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
    ),
)]
async fn delete_thread(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let thread = own_thread(&id, user.get_id(), &st.db).await?;
    service::rag_thread::delete(&st.db, thread).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Read a thread the caller owns, or `404`. A foreign id is indistinguishable
/// from a missing one.
async fn own_thread(id: &str, user: &UserId, db: &Database) -> Result<RagThread, AppError> {
    service::rag_thread::read_for(db, &RagThreadId::from_key(id), user)
        .await?
        .ok_or(AppError::NotFound)
}

// ---- turns ------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct SendRagMessage {
    /// What to ask. Required. `maxLength` here is the server's hard ceiling;
    /// the live cap is the school's `max_chatbot_message_len` (`GET /settings`),
    /// which is always at or below it — the same knob the chatbot nest uses.
    #[schema(
        example = "Newton'un ikinci yasasını açıklar mısın?",
        max_length = 8_000
    )]
    content: String,
}

/// The receipt for an accepted turn. The answer is *not* here: it is being
/// retrieved and generated. Poll `GET /rag/threads/{id}/messages/{mid}` or
/// open its `/stream`.
#[derive(Serialize, ToSchema)]
struct AcceptedResponse {
    /// The assistant row reserved for the answer.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    message_id: String,
    /// Always `pending` — that is what "accepted" means here.
    #[schema(example = "pending")]
    status: String,
}

/// One citation behind an answer, already resolved to something openable.
#[derive(Serialize, ToSchema)]
struct RagCitationResponse {
    /// The marker the answer text points at: `[N]` in `content` resolves to
    /// the citation whose `n` is `N`.
    #[schema(example = 1)]
    n: u32,
    /// The `course_note_file` record key the service's corpus `doc_id` was
    /// resolved through — fetch the file from
    /// `/course-note-files/{id}/content`. `null` while no file the asker may
    /// view claims that document (identical PDF bytes resolve to the same
    /// `doc_id`, so a citation may point at a file in a course this asker
    /// cannot read); the passage is still citable, just not openable.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    file: Option<String>,
    /// The page numbers within the document the passage spans.
    #[schema(example = json!([3, 4]))]
    pages: Vec<i64>,
    /// Opaque retrieval span ids within the document, for a client that
    /// highlights the exact passage.
    span_ids: Vec<String>,
    /// The subject the passage belongs to, when the corpus records one.
    #[schema(example = "Fizik")]
    ders: Option<String>,
}

impl From<&RagCitedFile> for RagCitationResponse {
    fn from(citation: &RagCitedFile) -> Self {
        Self {
            n: citation.n,
            file: citation.file.clone(),
            pages: citation.pages.clone(),
            span_ids: citation.span_ids.clone(),
            ders: citation.ders.clone(),
        }
    }
}

/// One turn. `content` is empty while `status` is `pending`; `error_code` is
/// set only when `status` is `failed`.
#[derive(Serialize, ToSchema)]
struct RagMessageResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    thread_id: String,
    /// `user` or `assistant`.
    #[schema(example = "assistant")]
    role: String,
    /// `pending`, `complete`, or `failed`. A user turn is always `complete`.
    #[schema(example = "complete")]
    status: String,
    content: String,
    /// `true` when the service declined to answer — a complete, successful
    /// turn, not a failure. `reason` says why (`guard_*`, `insufficient_data`,
    /// `model_abstained`, …).
    #[schema(example = false)]
    abstained: bool,
    /// The abstention's short machine code; `""` on an ordinary answer and on
    /// every user turn.
    #[schema(example = "insufficient_data")]
    reason: String,
    /// The passages the answer drew on, oldest first. Empty on a user turn,
    /// on a pending one and on a failed one.
    citations: Vec<RagCitationResponse>,
    /// Short machine-readable reason when `status` is `failed`:
    /// `unavailable`, `busy`, `timed_out`, `transport`, `protocol`,
    /// `bad_reply`, `empty_reply`, `interrupted`, or a code the AI service
    /// itself returned.
    #[schema(example = "timed_out")]
    error_code: Option<String>,
    /// UTC unix-milliseconds, server-stamped.
    created_at: i64,
    /// When the turn settled (complete or failed); `null` while pending.
    completed_at: Option<i64>,
}

impl RagMessageResponse {
    fn new(message: &RagMessage) -> Self {
        let reply = message.get_reply();
        Self {
            id: message.get_id().key().to_string(),
            thread_id: message.get_thread_id().key().to_string(),
            role: message.get_role().as_str().to_string(),
            status: message.get_status().as_str().to_string(),
            content: message.get_content().as_str().to_string(),
            abstained: reply.is_some_and(|reply| reply.abstained),
            reason: reply.map(|reply| reply.reason.clone()).unwrap_or_default(),
            citations: reply
                .map(|reply| reply.citations.iter().map(Into::into).collect())
                .unwrap_or_default(),
            error_code: message.get_error_code().map(str::to_string),
            created_at: message.get_created_at().as_millis(),
            completed_at: message.get_completed_at().map(|at| at.as_millis()),
        }
    }
}

/// The whole thread, oldest first. Paged via `?limit=&offset=`. Owner only.
#[utoipa::path(
    get,
    path = "/threads/{id}/messages",
    tag = "rag",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "RagThread id"), PageParams),
    responses(
        (status = 200, description = "A page of the thread's turns", body = Page<RagMessageResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
    ),
)]
async fn list_messages(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<RagMessageResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let thread = own_thread(&id, user.get_id(), &st.db).await?;
    let (messages, total) =
        service::rag_message::list_for_thread(&st.db, thread.get_id(), limit, offset).await?;
    let items = messages.iter().map(RagMessageResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Ask the corpus a question: `{content}`, at most the school's
/// `max_chatbot_message_len`. Answers `202 {message_id, status: "pending"}` the
/// moment both rows are written — the answer itself lands later, in the
/// reserved assistant row. `503` when no AI service offers `rag.chat`, and
/// nothing is written; `429` + `Retry-After` over the per-user send limit.
///
/// Order matters and is deliberate: ownership is settled first (a foreign
/// thread is a `404` and is never charged), then the per-user rate limit (a
/// refused turn leaves no trace), then availability (so an unavailable service
/// produces a `503` and no dead pending row), then the scope the question is
/// asked under — derived by [`crate::service::rag_scope`], never from the
/// body. Once the rows are written the turn always settles: the answering task
/// stamps `complete`/`failed`, a reader projects a long-stale `pending` as
/// failed, and the boot sweep repairs whatever a process death left behind.
#[utoipa::path(
    post,
    path = "/threads/{id}/messages",
    tag = "rag",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "RagThread id")),
    request_body = SendRagMessage,
    responses(
        (status = 202, description = "Accepted; the answer is on its way", body = AcceptedResponse),
        (status = 400, description = "Empty message, or longer than the school's `max_chatbot_message_len`, or a scope past `MAX_RAG_SCOPE_PAIRS`", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
        (status = 429, description = "Over the per-user send limit; retry after the advertised delay", body = ErrorResponse),
        (status = 503, description = "No AI service offers `rag.chat` right now; nothing was written", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn send_message(
    State(st): State<AppState>,
    SchoolIdCookie(school): SchoolIdCookie,
    tenant: ResolvedTenant,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SendRagMessage>,
) -> Result<Response, AppError> {
    // Ownership first: a foreign thread is a 404, never a 403 — and a 404 must
    // not spend the asker's minute.
    let thread = own_thread(&id, user.get_id(), &st.db).await?;

    // Then the tier. Charged before anything is written, so a refused turn
    // leaves no row behind.
    st.rag_limit
        .enforce_user(&scoped_key(&school, user.get_id().key().as_str()))?;

    let settings = service::settings::load(&st.db).await?;
    let reply_cap = content_cap(&settings);
    let length = req.content.chars().count();
    if length > reply_cap {
        return Err(AppError::Validation(ValidationError::TooLong {
            field: "content",
            max: reply_cap,
            got: length,
        }));
    }
    let content = RagContent::try_new(&req.content)?;

    // The 503 gate, and the only place it is evaluated. `has_capability` is
    // documented racy — that is fine here: it never guards a write, it only
    // spares the user a thread full of rows nothing could ever answer.
    let Some(bridge) = st.ai.clone() else {
        return Ok(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_RAG_CHAT_CAPABILITY) {
        return Ok(ai_unavailable("no AI service is connected right now"));
    }

    // The scope the corpus is routed by, derived on every question — a
    // manager or an admin gets every class-course pair in the school, everyone
    // else their own memberships — never from the body, so a student cannot
    // widen their own retrieval, and never from a stored copy, so a
    // membership change takes effect on the next question. An empty scope is
    // legal when the asker really has nothing to retrieve from, and is sent
    // as-is: the service abstains rather than the backend refusing a question
    // it cannot know is answerable. An admin's empty personal membership is
    // not that case.
    let scope = service::rag_scope::for_user(&st.db, &user, user.get_role()).await?;

    // Each create rides the thread's own row — it moves `updated_at` (the
    // list's sort key, so this is also the activity stamp) inside its own
    // transaction. That is what serializes a turn against `DELETE
    // /rag/threads/{id}`: the read above cannot, since reads do not conflict,
    // and a row written into the delete's window survived it as a turn under a
    // thread that is gone.
    let prompt =
        service::rag_message::append_user(&st.db, thread.get_id(), user.get_id(), content).await?;
    let answer =
        service::rag_message::append_pending_assistant(&st.db, thread.get_id(), user.get_id())
            .await?;

    let message_id = answer.get_id().key().to_string();
    // Never awaited inline: the retrieval-then-answer round trip can outlast
    // the request timeout that guards every handler, and the POST must return
    // now. The settings snapshot goes with it, so the whole turn is judged by
    // the policy that was live when it was accepted.
    // The prompt travels by id, not by write order: two POSTs on one thread
    // interleave across the two creates above, and picking "the newest user
    // row before this answer" out of the thread then answers the *other*
    // request's question twice.
    tokio::spawn(answer_turn(
        // The school and its database travel together — the same pair the
        // request was already resolved into, and one without the other is how
        // an answer lands in the wrong school.
        TenantExt {
            id: school,
            db: st.db.clone(),
            modules: tenant.modules,
        },
        bridge,
        prompt.get_id().clone(),
        answer,
        *user.get_id(),
        user.get_role(),
        scope,
        settings.get_chatbot_history_turns().max(0) as usize,
        reply_cap,
    ));

    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse {
            message_id,
            status: RagMessageStatus::Pending.as_str().to_string(),
        }),
    )
        .into_response())
}

/// The school's per-message character cap, never above the domain's hard
/// ceiling. Applied to what the user sends *and* to what the service answers —
/// the same knob the chatbot nest reads.
fn content_cap(settings: &Settings) -> usize {
    (settings.get_max_chatbot_message_len().max(0) as usize).min(MAX_CHATBOT_MESSAGE_LEN)
}

/// Fetch one turn's answer and settle its row. Runs detached from the request.
///
/// Every path settles: a row left `pending` would show as a spinner forever.
#[allow(clippy::too_many_arguments)] // the whole turn, handed to the detached task as one
async fn answer_turn(
    tenant: TenantExt,
    bridge: AiBridge,
    prompt_id: RagMessageId,
    answer: RagMessage,
    asker: UserId,
    asker_role: Role,
    scope: Vec<RagScopePair>,
    history_turns: usize,
    reply_cap: usize,
) {
    let db = tenant.db.clone();
    let answer_id = answer.get_id().clone();
    let thread = answer.get_thread_id().clone();
    // This request's own question, read back by id — the text is already
    // stored, so carrying the id rather than the string keeps the prompt and
    // its answer paired however two POSTs on one thread interleave.
    let prompt = match service::rag_message::prompt_of(&db, &prompt_id).await {
        Ok(Some(prompt)) => prompt,
        Ok(None) => {
            // The prompt row is gone — only reachable if the thread was deleted
            // out from under this task. There is nothing to ask, so settle.
            tracing::warn!("rag answer {} has no prompt to send", answer_id.key());
            let _ = service::rag_message::fail(&db, &answer_id, "internal").await;
            return;
        }
        Err(err) => {
            // Nothing to ask, and nothing will ask again: an unsettled row
            // would be a spinner until the staleness horizon. Settle it here.
            tracing::warn!("could not read the prompt for {}: {err}", answer_id.key());
            let _ = service::rag_message::fail(&db, &answer_id, "internal").await;
            return;
        }
    };
    let fresh = [prompt.get_id().clone(), answer_id.clone()];
    let history = match history_for(&db, &thread, &fresh, history_turns).await {
        Ok(history) => history,
        Err(err) => {
            tracing::warn!("could not load rag history: {err}");
            let _ = service::rag_message::fail(&db, &answer_id, "internal").await;
            return;
        }
    };
    let settled = match crate::ai::rag_chat::answer(
        &db,
        &bridge,
        &tenant.id,
        &thread,
        &fresh,
        prompt.get_content().as_str().to_string(),
        &asker,
        asker_role,
        scope,
        history,
    )
    .await
    {
        Ok((text, reply)) => {
            // The service is a trust boundary: cap what it wrote before it
            // reaches a row (or a browser). Truncation over rejection — a
            // clipped answer still helps, a discarded one does not.
            let clipped: String = text.chars().take(reply_cap).collect();
            if clipped.chars().count() < text.chars().count() {
                tracing::warn!(
                    "clipped an AI RAG reply to the school's {reply_cap}-character limit"
                );
            }
            match RagContent::try_new(&clipped) {
                Ok(content) => {
                    service::rag_message::complete(&db, &answer_id, content, SqlJson(reply)).await
                }
                Err(err) => {
                    // Nothing to show, and `RagContent` would refuse it anyway.
                    // A blank bubble is indistinguishable from a bug, so it is
                    // reported as one.
                    tracing::error!("a clipped AI RAG reply was still not storable: {err}");
                    service::rag_message::fail(&db, &answer_id, "bad_reply").await
                }
            }
        }
        Err(code) => {
            tracing::warn!("rag answer {} failed: {code}", answer_id.key());
            service::rag_message::fail(&db, &answer_id, &code).await
        }
    };
    match settled {
        Ok(_) => {}
        // The row was no longer `pending`: the boot sweep or a stale-timeout
        // already spoke for it, or the thread was deleted mid-flight. Benign —
        // the user is not waiting on this row any more.
        Err(AppError::NotFound) => {
            tracing::info!("rag answer {} was already settled", answer_id.key());
        }
        Err(err) => tracing::error!("could not settle rag answer {}: {err}", answer_id.key()),
    }
}

/// The tail of the thread replayed to the service: oldest first, only settled
/// turns with text, and never the two rows this turn just wrote (the new
/// prompt rides in `message`, the answer does not exist yet).
async fn history_for(
    db: &Database,
    thread: &RagThreadId,
    fresh: &[RagMessageId; 2],
    turns: usize,
) -> Result<Vec<crate::ai::rag_chat::RagTurn>, AppError> {
    // The query counts only settled rows with text, so a run of failed answers
    // makes the window reach *further back* instead of shrinking it — that is
    // what "the last `chatbot_history_turns` settled turns" means. +2 so
    // dropping this turn's own two rows cannot shorten it either (only the
    // user one can match: the assistant row is still `pending`).
    let tail =
        service::rag_message::list_settled_tail(db, thread, turns.saturating_add(2)).await?;
    let mut history: Vec<crate::ai::rag_chat::RagTurn> = tail
        .iter()
        .filter(|message| !fresh.contains(message.get_id()))
        .map(|message| crate::ai::rag_chat::RagTurn {
            role: match message.get_role() {
                RagMessageRole::User => ChatRole::User,
                RagMessageRole::Assistant => ChatRole::Assistant,
            },
            content: message.get_content().as_str().to_string(),
        })
        .collect();
    if history.len() > turns {
        history.drain(..history.len() - turns);
    }
    Ok(history)
}

/// Poll one turn. The non-SSE fallback for `/stream`, reading the same row —
/// including the projection that presents a long-stale `pending` as `failed`,
/// so the two can never disagree about a turn's state.
#[utoipa::path(
    get,
    path = "/threads/{id}/messages/{mid}",
    tag = "rag",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "RagThread id"),
        ("mid" = String, Path, description = "Message id"),
    ),
    responses(
        (status = 200, description = "The turn, in whatever state it is", body = RagMessageResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
    ),
)]
async fn read_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, mid)): Path<(String, String)>,
) -> Result<Json<RagMessageResponse>, AppError> {
    let message = own_message(&id, &mid, user.get_id(), &st.db).await?;
    Ok(Json(RagMessageResponse::new(&message)))
}

/// Read one turn only if the caller owns it *and* it belongs to the named
/// thread — a mismatched pair is a `404`, like a foreign one.
///
/// The thread is re-read rather than inferred from the message's `thread_id`,
/// so a turn whose thread was deleted reads as gone through these two routes
/// too — the same defence in depth [`super::chatbot`]'s twin carries.
async fn own_message(
    thread: &str,
    message: &str,
    user: &UserId,
    db: &Database,
) -> Result<RagMessage, AppError> {
    own_thread(thread, user, db).await?;
    service::rag_message::read_for(db, &RagMessageId::from_key(message), user)
        .await?
        .filter(|message| message.get_thread_id().key() == thread)
        .ok_or(AppError::NotFound)
}

/// Watch one turn as Server-Sent Events: `delta` chunks of the answer, then a
/// single `done` carrying the finished message, or one `error`. The stream
/// closes after `done`/`error` — one stream per turn, not per thread.
///
/// Works whatever state the turn is in when the stream opens: an answer that
/// already landed replays as `delta`s and a `done` straight away, so a client
/// that reconnects late is never left hanging. Consume with `EventSource`
/// (cookies ride along on same-site / credentialed requests). The `done`
/// payload carries the whole settled turn — text, abstention, citations — so a
/// late reader gets the citations too, not only the deltas.
#[utoipa::path(
    get,
    path = "/threads/{id}/messages/{mid}/stream",
    tag = "rag",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "RagThread id"),
        ("mid" = String, Path, description = "Message id"),
    ),
    responses(
        (status = 200, description = "SSE: `delta` (`{text}`) chunks, then `done` (`{message}`) or `error` (`{code, message}`)", content_type = "text/event-stream"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
    ),
)]
async fn stream_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, mid)): Path<(String, String)>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::Error>>>, AppError> {
    // Ownership is settled before the response becomes a stream — after this
    // point the only way to say "no" is an `error` event.
    own_message(&id, &mid, user.get_id(), &st.db).await?;

    let (tx, rx) = mpsc::channel(REPLY_CHUNKS + 2);
    let (message_id, user_id, db) = (
        RagMessageId::from_key(&mid),
        *user.get_id(),
        st.db.clone(),
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(CHAT_STREAM_POLL_MS));
        loop {
            // The first tick fires immediately: an already-finished turn is
            // delivered on connect rather than one poll interval later.
            //
            // The wait also watches the receiver, because a `pending` turn
            // sends nothing and would otherwise never learn that the client
            // hung up: axum drops the response body — and with it this
            // channel's receiver — the moment the connection dies, so this is
            // where an abandoned stream stops polling the database.
            tokio::select! {
                _ = ticker.tick() => {}
                _ = tx.closed() => return,
            }
            let message = match service::rag_message::read_for(&db, &message_id, &user_id).await {
                Ok(Some(message)) => message,
                // Deleted mid-stream (the thread went away) — say so and stop.
                Ok(None) => {
                    sse_error(&tx, "not_found", "this message no longer exists").await;
                    return;
                }
                Err(err) => {
                    tracing::warn!("rag stream read failed: {err}");
                    sse_error(&tx, "internal", "could not read the message").await;
                    return;
                }
            };

            match message.get_status() {
                // Includes the read-time projection: a `pending` row past its
                // staleness window arrives here as `failed`, so this loop
                // always terminates.
                RagMessageStatus::Pending => continue,
                RagMessageStatus::Complete => {
                    for chunk in slice_reply(message.get_content().as_str()) {
                        if sse_send(&tx, "delta", json!({ "text": chunk }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    let done = json!({ "message": RagMessageResponse::new(&message) });
                    let _ = sse_send(&tx, "done", done).await;
                    return;
                }
                RagMessageStatus::Failed => {
                    let code = message.get_error_code().unwrap_or("failed");
                    sse_error(&tx, code, "the assistant could not answer this message").await;
                    return;
                }
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

// ---- the one-shot study doors ----------------------------------------------
//
// Two capabilities of the same RAG service, each exactly one dispatch: a
// summary of one page/span range, and a set of practice questions over one.
// They are this nest's only doors that neither thread nor store — the artifact
// *is* the answer — so they wait for it, and no per-user rate limiter charges
// them: one-shot requests cannot be retried into a loop the way a thread can,
// and the RAG counts its own per-call rate and budget caps.
//
// Authorization is the nest's own scope derivation, applied twice: the asker's
// `(sinif, ders)` pairs come from [`service::rag_scope::for_user`] — never from
// the body — and the body only *names* a target inside them
// ([`resolve_study_pair`]). The pair that travels to the service is the derived
// one, so the corpus it retrieves from is a corpus the backend already decided
// the asker may study. The service's own scope guard is defence in depth: when
// it answers `role_required`/`scope_mismatch` the **backend** built a bad
// request, and that is reported as one (a `502`, logged by
// [`crate::ai::rag_study`]).

/// The scope half of a study request: which corpus, and which range inside it.
#[derive(Deserialize, ToSchema)]
struct StudyScopeRequest {
    /// The subject/course to study. Required, non-empty.
    ders: String,
    /// The şube/grade the corpus belongs to. Optional — a school-wide corpus
    /// (club/etüt) has none — and required in practice when the subject is
    /// taught at more than one grade: the door then answers `400` instead of
    /// guessing which section the caller meant.
    #[serde(default)]
    sinif: Option<String>,
    /// Pages of the corpus to work over. Normally exactly one of
    /// `pages`/`span_ids` is named; naming neither is answered by the service
    /// as an abstention (`empty_scope`), not refused here.
    #[serde(default)]
    pages: Vec<i64>,
    /// Retrieval span ids to work over, for a range the caller already holds
    /// from an earlier retrieval.
    #[serde(default)]
    span_ids: Vec<String>,
    /// A human label for the range (the heading the caller selected).
    #[serde(default)]
    scope_label: Option<String>,
}

impl StudyScopeRequest {
    /// The wire scope this request names, with the **resolved** pair as its
    /// `(sinif, ders)` half: the corpus the service is addressed to is the one
    /// [`crate::service::rag_scope`] authorized, never the body's spelling of
    /// it.
    fn into_scope(self, pair: RagScopePair) -> RagScope {
        RagScope {
            sinif: pair.sinif,
            ders: pair.ders,
            pages: self.pages,
            span_ids: self.span_ids,
            scope_label: self.scope_label.unwrap_or_default(),
        }
    }
}

/// A `/rag/questions` body: the same scope, plus the shape of the set to
/// generate.
#[derive(Deserialize, ToSchema)]
struct QuestionsRequest {
    #[serde(flatten)]
    scope: StudyScopeRequest,
    /// How many questions to generate. Defaults to 5; the door holds it to
    /// `1..=20`.
    #[serde(default = "default_question_count")]
    n: u32,
    /// The difficulty asked for, in the service's own vocabulary
    /// (`kolay`/`orta`/`zor`), defaulting to `orta`. Forwarded, never judged
    /// here: the vocabulary belongs to the service.
    #[serde(default = "default_difficulty")]
    difficulty: String,
    /// A question to base the set on, when the caller wants variations of one
    /// particular exercise. Omitted means "any question within the range".
    #[serde(default)]
    seed_question: Option<String>,
}

fn default_question_count() -> u32 {
    5
}

fn default_difficulty() -> String {
    "orta".to_string()
}

/// One citation behind a summary, as the bridge carries it. Unlike `rag.chat`'s
/// citations it names no corpus document, so nothing here resolves to a
/// course-note file: a summary is addressed to a range the caller selected.
#[derive(Serialize, ToSchema)]
struct RagSummaryCitationResponse {
    n: u32,
    pages: Vec<i64>,
    span_ids: Vec<String>,
}

impl From<RagSummaryCitation> for RagSummaryCitationResponse {
    fn from(citation: RagSummaryCitation) -> Self {
        Self {
            n: citation.n,
            pages: citation.pages,
            span_ids: citation.span_ids,
        }
    }
}

/// A summary of one range of one corpus. `abstained` with a `reason` is a
/// complete answer, exactly as it is on a chat turn.
#[derive(Serialize, ToSchema)]
struct RagSummarizeResponse {
    text: String,
    abstained: bool,
    reason: String,
    citations: Vec<RagSummaryCitationResponse>,
    /// The pages the summary actually covered.
    scope_pages: Vec<i64>,
    /// Whether the service summarized hierarchically (per section, then rolled
    /// up) rather than flat.
    hierarchical: bool,
}

impl From<RagSummarizeReply> for RagSummarizeResponse {
    fn from(reply: RagSummarizeReply) -> Self {
        Self {
            text: reply.text,
            abstained: reply.abstained,
            reason: reply.reason,
            citations: reply.citations.into_iter().map(Into::into).collect(),
            scope_pages: reply.scope_pages,
            hierarchical: reply.hierarchical,
        }
    }
}

/// One generated practice question. The bridge's `soru`/`cevap`/`zorluk` become
/// this API's `question`/`answer`/`difficulty` here: the wire keeps the
/// service's vocabulary, the HTTP surface keeps the backend's.
#[derive(Serialize, ToSchema)]
struct RagQuestionResponse {
    question: String,
    answer: String,
    difficulty: String,
}

impl From<RagQuestion> for RagQuestionResponse {
    fn from(question: RagQuestion) -> Self {
        Self {
            question: question.soru,
            answer: question.cevap,
            difficulty: question.zorluk,
        }
    }
}

/// A generated set of practice questions over one range of one corpus.
#[derive(Serialize, ToSchema)]
struct RagQuestionsResponse {
    items: Vec<RagQuestionResponse>,
    abstained: bool,
    reason: String,
    /// The retrieval spans the whole set is bounded to.
    span_ids: Vec<String>,
    /// The pages those spans sit on.
    pages: Vec<i64>,
}

impl From<RagQuestionsReply> for RagQuestionsResponse {
    fn from(reply: RagQuestionsReply) -> Self {
        Self {
            items: reply.items.into_iter().map(Into::into).collect(),
            abstained: reply.abstained,
            reason: reply.reason,
            span_ids: reply.span_ids,
            pages: reply.pages,
        }
    }
}

/// The one `(sınıf, ders)` pair a study request may address, resolved against
/// the asker's own derived pairs — the body only *names* a target inside them.
///
/// A named ders matches a pair **exactly** first — a picker's own spelling is
/// never re-rolled — and only then through [`crate::domain::text_fold::search_fold`],
/// the fold the RAG slug vocabulary rides: a citation names its course by slug
/// (`biyoloji`), the pair carries the course title (`Biyoloji`), and the two
/// spellings must resolve to one pair. A named grade must still be a pair the
/// asker holds; anything else is a `403`, because a corpus the asker cannot
/// study must not be summarized on their behalf. An absent or empty grade
/// resolves the subject alone: exactly one pair for that `ders` is the target,
/// none is a `403`, and more than one is a `400` — the caller must name the
/// şube, since picking one for them would answer from a corpus they did not
/// name. Two *distinct* courses folding to one key refuse the same way rather
/// than routing on a guess.
fn resolve_study_pair(
    pairs: &[RagScopePair],
    ders: &str,
    sinif: Option<&str>,
) -> Result<RagScopePair, AppError> {
    if ders.trim().is_empty() {
        return Err(AppError::Validation(ValidationError::Empty("ders")));
    }
    let grade = sinif.filter(|grade| !grade.trim().is_empty());
    let in_grade =
        |pair: &&RagScopePair| grade.is_none() || pair.sinif.as_deref() == grade;
    let mut matches: Vec<&RagScopePair> =
        pairs.iter().filter(|pair| in_grade(pair) && pair.ders == ders).collect();
    if matches.is_empty() {
        let folded = crate::domain::text_fold::search_fold(ders);
        matches = pairs
            .iter()
            .filter(|pair| {
                in_grade(pair) && crate::domain::text_fold::search_fold(&pair.ders) == folded
            })
            .collect();
    }
    let Some(first) = matches.first() else {
        return Err(AppError::Forbidden(match grade {
            Some(_) => "the requested ders/sinif pair is not in your scope",
            None => "the requested ders is not in your scope",
        }));
    };
    if matches.len() > 1 {
        // One title across several grades is the name-the-grade refusal;
        // distinct titles folding equal cannot be settled by naming anything,
        // so the ders itself is judged ambiguous — never a first-wins guess.
        let one_title = matches.iter().all(|pair| pair.ders == first.ders);
        return Err(AppError::Validation(ValidationError::Invalid {
            field: if one_title { "sinif" } else { "ders" },
            reason: if one_title {
                "name the grade: this ders is taught in more than one"
            } else {
                "more than one course matches this name; name the course exactly"
            },
        }));
    }
    Ok((*first).clone())
}

/// Summarize one range of one corpus the caller may study.
///
/// Synchronous by design: the summary is what the caller asked for, so there is
/// no thread to open, no row to poll and nothing stored. The scope is derived
/// by [`crate::service::rag_scope`] — a manager or an admin gets every
/// class-course pair in the school, everyone else their own memberships —
/// never from the body, which only names a target inside them — and the
/// `(sinif, ders)` pair that reaches the service is the derived one. `503`
/// when no AI service offers `rag.summarize`.
/// `abstained: true` with a `reason` is a complete answer riding a `200`.
///
/// The dispatch deadline (25 s) is deliberately under the middleware's own
/// 30 s envelope ([`crate::constant::REQUEST_TIMEOUT_SECS`]): this door waits,
/// and being dropped mid-flight would answer with prose written for a request
/// that may have written something.
#[utoipa::path(
    post,
    path = "/summarize",
    tag = "rag",
    security(("session_cookie" = [])),
    request_body = StudyScopeRequest,
    responses(
        (status = 200, description = "The summary — possibly an abstention: `abstained: true` with a `reason` is a complete answer", body = RagSummarizeResponse),
        (status = 400, description = "`ders` is empty, or `sinif` was omitted for a ders taught in more than one grade", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "The requested `(sinif, ders)` is not one of the caller's own scopes", body = ErrorResponse),
        (status = 409, description = "The AI service refused to run the request — its model is unavailable, or the request was over its rate or budget cap", body = ErrorResponse),
        (status = 502, description = "The AI service answered something this backend could not use, or built a request it refused", body = ErrorResponse),
        (status = 503, description = "No AI service offers `rag.summarize` right now", body = ErrorResponse),
        (status = 504, description = "The AI service did not answer within the dispatch deadline", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn summarize(
    State(st): State<AppState>,
    SchoolIdCookie(school): SchoolIdCookie,
    CurrentUser(user): CurrentUser,
    Json(req): Json<StudyScopeRequest>,
) -> Result<Response, AppError> {
    let Some(bridge) = st.ai.clone() else {
        return Ok(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_RAG_SUMMARIZE_CAPABILITY) {
        return Ok(ai_unavailable("no AI service is connected right now"));
    }

    // The scope the corpus is routed by, derived on every request — a manager
    // or an admin gets every class-course pair in the school, everyone else
    // their own memberships — never from the body, so a caller cannot
    // summarize a corpus they cannot study. The body's `ders`/`sinif` only
    // pick a pair out of that set.
    let pairs = service::rag_scope::for_user(&st.db, &user, user.get_role()).await?;
    let pair = resolve_study_pair(&pairs, &req.ders, req.sinif.as_deref())?;
    let payload = RagSummarizePayload {
        scope: req.into_scope(pair),
        scope_pairs: pairs
            .iter()
            .map(|pair| (pair.sinif.clone(), pair.ders.clone()))
            .collect(),
        // What the session already proves, never what the body claims.
        asker: user.get_id().key(),
        asker_role: user.get_role().as_str().to_string(),
    };
    match rag_study::summarize(&bridge, &school, payload).await {
        Ok(reply) => Ok(Json(RagSummarizeResponse::from(reply)).into_response()),
        Err(code) => Ok(failure(&code)),
    }
}

/// Generate practice questions over one range of one corpus the caller may
/// study, with the answers bounded to that range.
///
/// One-shot and synchronous like [`summarize`], and gated the same way: the
/// same derived scope, the same `403`/`400` resolution, the same `503` when no
/// service offers the capability, and the same rule that an abstention is a
/// complete answer on a `200`. What it adds is the shape of the set: `n`
/// (default 5, at most [`MAX_RAG_QUESTIONS`]), `difficulty` (default `orta`)
/// and an optional `seed_question` to generate variations of one exercise.
#[utoipa::path(
    post,
    path = "/questions",
    tag = "rag",
    security(("session_cookie" = [])),
    request_body = QuestionsRequest,
    responses(
        (status = 200, description = "The generated questions — possibly an abstention: `abstained: true` with a `reason` is a complete answer", body = RagQuestionsResponse),
        (status = 400, description = "`ders` is empty, `sinif` was omitted for a ders taught in more than one grade, or `n` is outside `1..=20`", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "The requested `(sinif, ders)` is not one of the caller's own scopes", body = ErrorResponse),
        (status = 409, description = "The AI service refused to run the request — its model is unavailable, or the request was over its rate or budget cap", body = ErrorResponse),
        (status = 502, description = "The AI service answered something this backend could not use, or built a request it refused", body = ErrorResponse),
        (status = 503, description = "No AI service offers `rag.questions` right now", body = ErrorResponse),
        (status = 504, description = "The AI service did not answer within the dispatch deadline", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn questions(
    State(st): State<AppState>,
    SchoolIdCookie(school): SchoolIdCookie,
    CurrentUser(user): CurrentUser,
    Json(req): Json<QuestionsRequest>,
) -> Result<Response, AppError> {
    let Some(bridge) = st.ai.clone() else {
        return Ok(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_RAG_QUESTIONS_CAPABILITY) {
        return Ok(ai_unavailable("no AI service is connected right now"));
    }

    let pairs = service::rag_scope::for_user(&st.db, &user, user.get_role()).await?;
    let pair = resolve_study_pair(&pairs, &req.scope.ders, req.scope.sinif.as_deref())?;
    if !(1..=MAX_RAG_QUESTIONS).contains(&req.n) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "n",
            reason: "must be between 1 and 20",
        }));
    }
    let payload = RagQuestionsPayload {
        scope: req.scope.into_scope(pair),
        scope_pairs: pairs
            .iter()
            .map(|pair| (pair.sinif.clone(), pair.ders.clone()))
            .collect(),
        asker: user.get_id().key(),
        asker_role: user.get_role().as_str().to_string(),
        n: req.n,
        difficulty: req.difficulty,
        seed_question: req.seed_question,
    };

    match rag_study::questions(&bridge, &school, payload).await {
        Ok(reply) => Ok(Json(RagQuestionsResponse::from(reply)).into_response()),
        Err(code) => Ok(failure(&code)),
    }
}

/// A relayed failure as the response a client branches on: the service's own
/// code decides the status where it is one this backend knows, and anything
/// else is a `502` — the backend cannot vouch for an answer it did not
/// understand, and a gateway error is exactly that claim.
///
/// `role_required` and `scope_mismatch` land in that fallback deliberately:
/// both mean the **backend** composed a request the service may not answer, so
/// [`crate::ai::rag_study`] logs them as bugs where they are detected and the
/// caller is told the gateway failed rather than being handed the service's own
/// wording for this side's defect.
fn failure(code: &str) -> Response {
    let status = match code {
        "bad_request" => StatusCode::BAD_REQUEST,
        "not_found" => StatusCode::NOT_FOUND,
        "not_ready" | "llm_unavailable" => StatusCode::CONFLICT,
        "busy" | "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "timed_out" => StatusCode::GATEWAY_TIMEOUT,
        "internal" => StatusCode::INTERNAL_SERVER_ERROR,
        // `transport`, `protocol`, `bad_reply`, `role_required`,
        // `scope_mismatch`, `service_error`, and any code the service
        // invented: a bad gateway.
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": code }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_citation_reply_maps_every_field_into_the_wire_shape() {
        let stored = RagCitedFile {
            n: 2,
            file: Some("0197-file".into()),
            pages: vec![3, 4],
            span_ids: vec!["s-1".into()],
            ders: Some("Fizik".into()),
        };
        let wire = RagCitationResponse::from(&stored);
        assert_eq!(wire.n, 2);
        assert_eq!(wire.file.as_deref(), Some("0197-file"));
        assert_eq!(wire.pages, vec![3, 4]);
        assert_eq!(wire.span_ids, vec!["s-1"]);
        assert_eq!(wire.ders.as_deref(), Some("Fizik"));

        // An unresolved citation keeps every other field — the passage is
        // still citable, only not openable.
        let unresolved = RagCitationResponse::from(&RagCitedFile {
            file: None,
            ..stored
        });
        assert_eq!(unresolved.file, None);
        assert_eq!(unresolved.pages, vec![3, 4]);
    }

    fn pair(sinif: Option<&str>, ders: &str) -> RagScopePair {
        RagScopePair {
            sinif: sinif.map(str::to_string),
            ders: ders.to_string(),
        }
    }

    #[test]
    fn a_study_pair_resolves_only_inside_the_askers_own_scope() {
        let pairs = vec![
            pair(Some("10"), "Biyoloji"),
            pair(None, "Satranç Kulübü"),
            pair(Some("9"), "Matematik"),
        ];

        // A named grade must be the exact pair, and the resolved pair is what
        // travels — never the body's own spelling.
        let resolved = resolve_study_pair(&pairs, "Biyoloji", Some("10")).unwrap();
        assert_eq!(resolved.sinif.as_deref(), Some("10"));
        let grade_less = resolve_study_pair(&pairs, "Satranç Kulübü", None).unwrap();
        assert_eq!(grade_less.sinif, None);

        // A grade the asker is not scoped to, and a ders they hold no pair
        // for, are both `403` — a corpus they cannot study must not be
        // summarized on their behalf.
        assert!(matches!(
            resolve_study_pair(&pairs, "Biyoloji", Some("11")),
            Err(AppError::Forbidden(_))
        ));
        assert!(matches!(
            resolve_study_pair(&pairs, "Fizik", None),
            Err(AppError::Forbidden(_))
        ));
        // An empty ders is a malformed body, not a scope refusal.
        assert!(matches!(
            resolve_study_pair(&pairs, "  ", None),
            Err(AppError::Validation(ValidationError::Empty("ders")))
        ));

        // Two şubeler teaching one course: the caller must name the grade,
        // because picking one for them answers from a corpus they did not name.
        let two = vec![pair(Some("9"), "Fizik"), pair(Some("10"), "Fizik")];
        assert!(matches!(
            resolve_study_pair(&two, "Fizik", None),
            Err(AppError::Validation(ValidationError::Invalid { field: "sinif", .. }))
        ));
        assert!(resolve_study_pair(&two, "Fizik", Some("10")).is_ok());
    }
    /// A citation names its course by the RAG slug vocabulary (lowercase,
    /// diacritic-stripped) while the pair carries the course title: the body's
    /// `biyoloji` must resolve the pair whose title is `Biyoloji`, with the
    /// derived title — never the body's slug — being what travels.
    #[test]
    fn a_study_pair_matches_the_slug_spelling_through_the_search_fold() {
        let pairs = vec![
            pair(Some("10"), "Biyoloji"),
            pair(Some("9"), "İngilizce"),
            pair(Some("11"), "Coğrafya"),
        ];

        // The slug spelling resolves, with and without a named grade.
        let resolved = resolve_study_pair(&pairs, "biyoloji", None).unwrap();
        assert_eq!(resolved.ders, "Biyoloji");
        assert_eq!(resolved.sinif.as_deref(), Some("10"));
        assert_eq!(
            resolve_study_pair(&pairs, "biyoloji", Some("10")).unwrap().ders,
            "Biyoloji"
        );

        // The fold is Turkish-correct both ways: dotted İ folds, and the
        // slug's stripped diacritics still reach the accented title.
        assert_eq!(
            resolve_study_pair(&pairs, "ingilizce", None).unwrap().ders,
            "İngilizce"
        );
        assert_eq!(
            resolve_study_pair(&pairs, "cografya", Some("11")).unwrap().ders,
            "Coğrafya"
        );

        // A grade the asker holds no folded pair for, and a ders no course
        // folds to, are still the same scope refusal — never widened access.
        assert!(matches!(
            resolve_study_pair(&pairs, "biyoloji", Some("11")),
            Err(AppError::Forbidden(_))
        ));
        assert!(matches!(
            resolve_study_pair(&pairs, "fizik", None),
            Err(AppError::Forbidden(_))
        ));
    }

    /// Two *distinct* courses folding to one key refuse rather than route on
    /// a guess — the same multi-match verdict family as the multi-grade
    /// refusal, judged on `ders` since naming a grade cannot settle it.
    #[test]
    fn folded_ambiguity_refuses_instead_of_guessing() {
        // `Kâğıt` and `Kagit` fold to one key, and no exact spelling of the
        // slug settles which one the caller meant.
        let folded_twins = vec![pair(Some("9"), "Kâğıt"), pair(Some("9"), "Kagit")];
        assert!(matches!(
            resolve_study_pair(&folded_twins, "kagit", None),
            Err(AppError::Validation(ValidationError::Invalid { field: "ders", .. }))
        ));
        assert!(matches!(
            resolve_study_pair(&folded_twins, "kagit", Some("9")),
            Err(AppError::Validation(ValidationError::Invalid { field: "ders", .. }))
        ));

        // One course taught in two grades, named by slug: the caller must
        // still name the grade.
        let two_grades = vec![pair(Some("9"), "Biyoloji"), pair(Some("10"), "Biyoloji")];
        assert!(matches!(
            resolve_study_pair(&two_grades, "biyoloji", None),
            Err(AppError::Validation(ValidationError::Invalid { field: "sinif", .. }))
        ));
        assert_eq!(
            resolve_study_pair(&two_grades, "biyoloji", Some("10")).unwrap().ders,
            "Biyoloji"
        );
    }
}
