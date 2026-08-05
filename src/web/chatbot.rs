//! The chatbot relay: browser ⇄ backend ⇄ AI service (over the QUIC bridge).
//!
//! The backend owns everything except the answer itself — auth, the per-user
//! rate limit, the thread, the payload format, and the last look at the reply
//! before it is stored. There are no intent or rule tables: the thread is
//! free-form on purpose.
//!
//! Sending is asynchronous by design. `POST .../messages` writes the user's
//! turn plus an empty `pending` assistant row and answers `202` immediately;
//! an inference can outlast the request-timeout layer that guards every HTTP
//! handler, so nothing is awaited on the request path. The client then either
//! polls `GET .../messages/{mid}` or opens its SSE stream — both read the same
//! row, so they can never disagree.
//!
//! The bridge round trip runs in a `tokio::spawn` the POST leaves behind. A
//! restart therefore orphans a turn in flight: its row stays `pending` until a
//! reader projects it failed, and the boot sweep stamps that verdict durably.
//! Acceptable because the backend is one process with stop-the-world deploys —
//! there is no peer that could have picked the turn up anyway.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::chat::{ChatReplyPayload, ChatRequestPayload, ChatRole, ChatTurn};
use crate::ai::{AiBridge, AiError};
use crate::constant::{
    AI_CHAT_CAPABILITY, CHAT_STREAM_POLL_MS, MAX_CHATBOT_MESSAGE_LEN, MIN_CHUNK_CHARS, REPLY_CHUNKS,
};
use crate::database::Database;
use crate::domain::chatbot_message::{
    ChatContent, ChatbotMessage, ChatbotMessageId, MessageRole, MessageStatus,
};
use crate::domain::chatbot_thread::{ChatbotThread, ChatbotThreadId, ChatbotThreadTitle};
use crate::domain::settings::Settings;
use crate::domain::user::UserId;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::{CurrentUser, Page, PageParams};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_thread, list_threads))
        .routes(routes!(rename_thread, delete_thread))
        .routes(routes!(list_messages, send_message))
        .routes(routes!(read_message))
        .routes(routes!(stream_message))
}

// ---- threads ----------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct CreateChatbotThread {
    /// Optional thread name, up to 200 characters. Blank counts as absent —
    /// an untitled thread is normal (the UI labels it from its first turn).
    #[schema(max_length = 200, example = "Fizik ödevi")]
    title: Option<String>,
}

/// One chatbot thread. `updated_at` moves on every turn, and the list is
/// sorted by it, so the thread just written to is always first.
#[derive(Serialize, ToSchema)]
struct ChatbotThreadResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    #[schema(example = "Fizik ödevi")]
    title: Option<String>,
    /// UTC unix-milliseconds, server-stamped.
    created_at: i64,
    /// UTC unix-milliseconds of the last turn, server-stamped.
    updated_at: i64,
}

impl ChatbotThreadResponse {
    fn new(thread: &ChatbotThread) -> Self {
        Self {
            id: thread.get_id().key().to_string(),
            title: thread.get_title().map(|title| title.as_str().to_string()),
            created_at: thread.get_created_at().as_millis(),
            updated_at: thread.get_updated_at().as_millis(),
        }
    }
}

/// Start a new chatbot thread, optionally named. Every authenticated role may
/// chat, parents included. A user may keep up to the school's
/// `max_chatbot_threads` threads; at the cap the request is refused (409)
/// until an old thread is deleted — the cap is storage protection, not a
/// usage quota (that is the per-minute message limit).
#[utoipa::path(
    post,
    path = "/threads",
    tag = "chatbot",
    security(("session_cookie" = [])),
    request_body = CreateChatbotThread,
    responses(
        (status = 201, description = "The new thread", body = ChatbotThreadResponse),
        (status = 400, description = "Invalid title", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 409, description = "At the school's thread cap", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_thread(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<CreateChatbotThread>,
) -> Result<(StatusCode, Json<ChatbotThreadResponse>), AppError> {
    // Blank is "untitled", not an error — same rule as a message's label.
    let title = match req.title.as_deref().map(str::trim).unwrap_or_default() {
        "" => None,
        value => Some(ChatbotThreadTitle::try_new(value)?),
    };

    // The cap is checked and the row written as one critical section in the
    // domain: counting here and creating after would over-admit under
    // concurrency (the database does not serialize a count against inserts).
    let thread = ChatbotThread::create_capped(user.get_id(), title, &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(ChatbotThreadResponse::new(&thread)),
    ))
}

/// The caller's own threads, most recently active first. Paged via
/// `?limit=&offset=`. Nobody — no teacher, no admin — reads anyone else's.
#[utoipa::path(
    get,
    path = "/threads",
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's threads", body = Page<ChatbotThreadResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_threads(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ChatbotThreadResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (threads, total) =
        ChatbotThread::list_for_user(user.get_id(), limit, offset, &st.db).await?;
    let items = threads.iter().map(ChatbotThreadResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[derive(Deserialize, ToSchema)]
struct RenameChatbotThread {
    /// The new name, up to 200 characters. `null` — or blank — clears it back
    /// to untitled, the same rule the create route applies.
    #[schema(max_length = 200, example = "Fizik ödevi")]
    title: Option<String>,
}

/// Rename a thread, or clear its name (`title: null`). Owner only; someone
/// else's thread is a `404`, never a `403`. The edit counts as activity, so the
/// thread moves to the top of the list — renaming is how a user files a thread,
/// and a rename that left it buried would be useless.
///
/// Nothing names a thread automatically: the first message is not turned into a
/// title, and the AI service is never asked for one.
#[utoipa::path(
    patch,
    path = "/threads/{id}",
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "ChatbotThread id")),
    request_body = RenameChatbotThread,
    responses(
        (status = 200, description = "The renamed thread", body = ChatbotThreadResponse),
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
    Json(req): Json<RenameChatbotThread>,
) -> Result<Json<ChatbotThreadResponse>, AppError> {
    let title = match req.title.as_deref().map(str::trim).unwrap_or_default() {
        "" => None,
        value => Some(ChatbotThreadTitle::try_new(value)?),
    };
    let thread = own_thread(&id, user.get_id(), &st.db).await?;
    let renamed = thread.rename(title, &st.db).await?;
    Ok(Json(ChatbotThreadResponse::new(&renamed)))
}

/// Delete a thread and every turn in it, permanently. Owner only; someone
/// else's thread is a `404`, never a `403` (its existence is not leaked).
#[utoipa::path(
    delete,
    path = "/threads/{id}",
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "ChatbotThread id")),
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
    thread.delete(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Read a thread the caller owns, or `404`. A foreign id is indistinguishable
/// from a missing one.
async fn own_thread(id: &str, user: &UserId, db: &Database) -> Result<ChatbotThread, AppError> {
    ChatbotThread::read_for(&ChatbotThreadId::from_key(id), user, db)
        .await?
        .ok_or(AppError::NotFound)
}

// ---- turns ------------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct SendChatbotMessage {
    /// What to ask. Required. `maxLength` here is the server's hard ceiling;
    /// the live cap is the school's `max_chatbot_message_len` (`GET /settings`),
    /// which is always at or below it.
    #[schema(
        example = "Newton'un ikinci yasasını açıklar mısın?",
        max_length = 8_000
    )]
    content: String,
}

/// The receipt for an accepted turn. The answer is *not* here: it is being
/// fetched. Poll `GET /chatbot/threads/{id}/messages/{mid}` or open its
/// `/stream`.
#[derive(Serialize, ToSchema)]
struct AcceptedResponse {
    /// The assistant row reserved for the answer.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    message_id: String,
    /// Always `pending` — that is what "accepted" means here.
    #[schema(example = "pending")]
    status: String,
}

/// One turn. `content` is empty while `status` is `pending`; `error_code` is
/// set only when `status` is `failed`.
#[derive(Serialize, ToSchema)]
struct ChatbotMessageResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    thread_id: String,
    /// `user` or `assistant`.
    #[schema(example = "assistant")]
    role: String,
    /// `pending`, `complete`, or `failed`. A user turn is always `complete`.
    #[schema(example = "complete")]
    status: String,
    content: String,
    /// `true` when `content` is only the first part of what the assistant
    /// answered — the rest was over the school's `max_chatbot_message_len` and was
    /// cut. Always `false` for a user turn and for a failed one. Show the user
    /// that the answer is incomplete; asking again shortens nothing, so the way
    /// out is a narrower question (or a bigger cap).
    #[schema(example = false)]
    truncated: bool,
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

impl ChatbotMessageResponse {
    fn new(message: &ChatbotMessage) -> Self {
        Self {
            id: message.get_id().key().to_string(),
            thread_id: message.get_thread_id().key().to_string(),
            role: message.get_role().as_str().to_string(),
            status: message.get_status().as_str().to_string(),
            content: message.get_content().as_str().to_string(),
            truncated: message.is_truncated(),
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
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "ChatbotThread id"), PageParams),
    responses(
        (status = 200, description = "A page of the thread's turns", body = Page<ChatbotMessageResponse>),
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
) -> Result<Json<Page<ChatbotMessageResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let thread = own_thread(&id, user.get_id(), &st.db).await?;
    let (messages, total) =
        ChatbotMessage::list_for_thread(thread.get_id(), limit, offset, &st.db).await?;
    let items = messages.iter().map(ChatbotMessageResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Ask the chatbot. Answers `202` the moment both rows are written — the
/// answer itself lands later, in the reserved assistant row.
///
/// Order matters and is deliberate: the per-user rate limit is charged
/// *before* anything is written, so a refused turn leaves no trace;
/// availability is checked *before* the rows exist, so an unavailable service
/// produces a `503` and no dead pending row. Once the rows are written the turn
/// always settles — the answering task stamps `complete`/`failed`, a reader
/// projects a long-stale `pending` as failed, and the boot sweep repairs
/// whatever a process death left behind.
#[utoipa::path(
    post,
    path = "/threads/{id}/messages",
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "ChatbotThread id")),
    request_body = SendChatbotMessage,
    responses(
        (status = 202, description = "Accepted; the answer is on its way", body = AcceptedResponse),
        (status = 400, description = "Empty message, or longer than the school's `max_chatbot_message_len`", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
        (status = 429, description = "Over the per-user message rate limit; see `Retry-After`", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
        (status = 503, description = "No AI service is available — nothing was written, retry later", body = ErrorResponse),
    ),
)]
async fn send_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SendChatbotMessage>,
) -> Result<Response, AppError> {
    // Charged first: a rejected turn must cost nothing and leave no row.
    st.chatbot_limit.enforce_user(user.get_id().key())?;

    let thread = own_thread(&id, user.get_id(), &st.db).await?;
    let settings = Settings::load(&st.db).await?;
    let reply_cap = content_cap(&settings);
    let length = req.content.chars().count();
    if length > reply_cap {
        return Err(AppError::Validation(ValidationError::TooLong {
            field: "content",
            max: reply_cap,
            got: length,
        }));
    }
    let content = ChatContent::try_new(&req.content)?;

    // The 503 gate, and the only place it is evaluated. `has_capability` is
    // documented racy — that is fine here: it never guards a write, it only
    // spares the user a thread full of rows nothing could ever answer.
    let Some(bridge) = st.ai.clone() else {
        return Ok(unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_CHAT_CAPABILITY) {
        return Ok(unavailable("no AI service is connected right now"));
    }

    // Each create rides the thread's own row — it moves `updated_at` (the
    // list's sort key, so this is also the activity stamp) inside its own
    // transaction. That is what serializes a turn against `DELETE
    // /chatbot/threads/{id}`: the read above cannot, since reads do not
    // conflict, and a row written into the delete's window survived it as a
    // turn under a thread that is gone. A thread deleted between the two
    // creates therefore refuses the second one with a 404 — and the first is
    // swept by that same delete, so the turn leaves no half of itself behind.
    let prompt =
        ChatbotMessage::append_user(thread.get_id(), user.get_id(), content, &st.db).await?;
    let answer =
        ChatbotMessage::append_pending_assistant(thread.get_id(), user.get_id(), &st.db).await?;

    let message_id = answer.get_id().key().to_string();
    // Never awaited inline: the bridge round trip can outlast the request
    // timeout that guards every handler, and the POST must return now. The
    // settings snapshot goes with it, so the whole turn is judged by the policy
    // that was live when it was accepted.
    // The prompt travels by id, not by write order: two POSTs on one thread
    // interleave across the two creates above, and picking "the newest user row
    // before this answer" out of the thread then answers the *other* request's
    // question twice.
    tokio::spawn(answer_turn(
        bridge,
        st.db.clone(),
        prompt.get_id().clone(),
        answer,
        settings.get_chatbot_history_turns().max(0) as usize,
        reply_cap,
    ));

    Ok((
        StatusCode::ACCEPTED,
        Json(AcceptedResponse {
            message_id,
            status: MessageStatus::Pending.as_str().to_string(),
        }),
    )
        .into_response())
}

/// The school's per-message character cap, never above the domain's hard
/// ceiling. Applied to what the user sends *and* to what the service answers.
fn content_cap(settings: &Settings) -> usize {
    (settings.get_max_chatbot_message_len().max(0) as usize).min(MAX_CHATBOT_MESSAGE_LEN)
}

/// The AI-unavailable `503`. Built here rather than as an `AppError` variant:
/// this is the only route that can produce it, and `AppError`'s 503s all mean
/// "the database is reconnecting", which this is not. No `Retry-After` — when
/// a service will dial back in is unknowable.
fn unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// Fetch one turn's answer and settle its row. Runs detached from the request.
///
/// Every path settles: a row left `pending` would show as a spinner forever.
async fn answer_turn(
    bridge: AiBridge,
    db: Database,
    prompt_id: ChatbotMessageId,
    answer: ChatbotMessage,
    history_turns: usize,
    reply_cap: usize,
) {
    let answer_id = answer.get_id().clone();
    let thread = answer.get_thread_id().clone();
    // This request's own question, read back by id — the text is already
    // stored, so carrying the id rather than the string keeps the prompt and
    // its answer paired however two POSTs on one thread interleave.
    let prompt = match ChatbotMessage::prompt_of(&prompt_id, &db).await {
        Ok(Some(prompt)) => prompt,
        Ok(None) => {
            // The prompt row is gone — only reachable if the thread was deleted
            // out from under this task. There is nothing to ask, so settle.
            tracing::warn!("chat answer {} has no prompt to send", answer_id.key());
            let _ = ChatbotMessage::fail(&answer_id, "internal", &db).await;
            return;
        }
        Err(err) => {
            // Nothing to ask, and nothing will ask again: the claim queue that
            // used to reclaim an unsettled row is gone, so leaving it `pending`
            // means a spinner until the 300s stale horizon. Settle it here.
            tracing::warn!("could not read the prompt for {}: {err}", answer_id.key());
            let _ = ChatbotMessage::fail(&answer_id, "internal", &db).await;
            return;
        }
    };
    let fresh = [prompt.get_id().clone(), answer_id.clone()];
    let settled = match fetch_reply(
        &bridge,
        &db,
        &thread,
        &fresh,
        prompt.get_content().as_str().to_string(),
        history_turns,
        reply_cap,
    )
    .await
    {
        Ok((text, truncated)) => ChatbotMessage::complete(&answer_id, text, truncated, &db).await,
        Err(code) => {
            tracing::warn!("chat answer {} failed: {code}", answer_id.key());
            ChatbotMessage::fail(&answer_id, &code, &db).await
        }
    };
    match settled {
        Ok(_) => {}
        // The row was no longer `pending`: the boot sweep or a stale-timeout
        // already spoke for it, or the thread was deleted mid-flight.
        // Benign — the user is not waiting on this row any more.
        Err(AppError::NotFound) => {
            tracing::info!("chat answer {} was already settled", answer_id.key());
        }
        Err(err) => tracing::error!("could not settle chat answer {}: {err}", answer_id.key()),
    }
}

/// The bridge round trip, with every failure mapped to a stable code.
async fn fetch_reply(
    bridge: &AiBridge,
    db: &Database,
    thread: &ChatbotThreadId,
    fresh: &[ChatbotMessageId; 2],
    prompt: String,
    history_turns: usize,
    reply_cap: usize,
) -> Result<(ChatContent, bool), String> {
    let history = match history_for(db, thread, fresh, history_turns).await {
        Ok(history) => history,
        Err(err) => {
            tracing::warn!("could not load chat history: {err}");
            return Err("internal".to_string());
        }
    };
    let payload = serde_json::to_value(ChatRequestPayload {
        message: prompt,
        history,
    })
    .map_err(|err| {
        tracing::error!("could not encode a chat request: {err}");
        "internal".to_string()
    })?;

    let raw = bridge
        .dispatch(AI_CHAT_CAPABILITY, payload)
        .await
        .map_err(failure_code)?;
    let reply: ChatReplyPayload = serde_json::from_value(raw).map_err(|err| {
        tracing::warn!("AI service answered with an unreadable chat payload: {err}");
        "bad_reply".to_string()
    })?;

    // The service is a trust boundary: cap what it wrote before it reaches a
    // row (or a browser). Truncation over rejection — a clipped answer still
    // helps, a discarded one does not — but the clip is recorded, so the UI can
    // tell the user the text was cut instead of passing it off as the whole
    // answer.
    let text: String = reply.text.chars().take(reply_cap).collect();
    if text.trim().is_empty() {
        // Nothing to show, and `ChatContent` would refuse it anyway. A blank
        // bubble is indistinguishable from a bug, so it is reported as one.
        return Err("empty_reply".to_string());
    }
    let truncated = text.chars().count() < reply.text.chars().count();
    if truncated {
        tracing::warn!("clipped an AI chat reply to the school's {reply_cap}-character limit");
    }
    let content = ChatContent::try_new(&text).map_err(|err| {
        tracing::error!("a clipped AI reply was still not storable: {err}");
        "bad_reply".to_string()
    })?;
    Ok((content, truncated))
}

/// The tail of the thread replayed to the service: oldest first, only
/// settled turns with text, and never the two rows this turn just wrote (the
/// new prompt rides in `message`, the answer does not exist yet).
async fn history_for(
    db: &Database,
    thread: &ChatbotThreadId,
    fresh: &[ChatbotMessageId; 2],
    turns: usize,
) -> Result<Vec<ChatTurn>, AppError> {
    // The query counts only settled rows with text, so a run of failed answers
    // makes the window reach *further back* instead of shrinking it — that is
    // what "the last `chatbot_history_turns` settled turns" means. +2 so dropping
    // this turn's own two rows cannot shorten it either (only the user one can
    // match: the assistant row is still `pending`).
    let tail = ChatbotMessage::list_settled_tail(thread, turns.saturating_add(2), db).await?;
    let mut history: Vec<ChatTurn> = tail
        .iter()
        .filter(|message| !fresh.contains(message.get_id()))
        .map(|message| ChatTurn {
            role: match message.get_role() {
                MessageRole::User => ChatRole::User,
                MessageRole::Assistant => ChatRole::Assistant,
            },
            content: message.get_content().as_str().to_string(),
        })
        .collect();
    if history.len() > turns {
        history.drain(..history.len() - turns);
    }
    Ok(history)
}

/// A dispatch failure as a short, stable code the frontend can branch on.
fn failure_code(err: AiError) -> String {
    match err {
        AiError::NoWorker(_) | AiError::Setup(_) => "unavailable".to_string(),
        AiError::Busy(_) => "busy".to_string(),
        AiError::Timeout(_) => "timed_out".to_string(),
        AiError::Transport(_) => "transport".to_string(),
        AiError::Protocol(_) | AiError::IdMismatch { .. } => "protocol".to_string(),
        // The service's own verdict. Kept verbatim (the domain trims it) so a
        // service can define codes the backend has never heard of.
        AiError::Remote { code, message } => {
            tracing::warn!("AI chat service refused the request: {code}: {message}");
            if code.trim().is_empty() {
                "service_error".to_string()
            } else {
                code
            }
        }
    }
}

/// Poll one turn. The non-SSE fallback for `/stream`, reading the same row —
/// including the projection that presents a long-stale `pending` as `failed`,
/// so the two can never disagree about a turn's state.
#[utoipa::path(
    get,
    path = "/threads/{id}/messages/{mid}",
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "ChatbotThread id"),
        ("mid" = String, Path, description = "Message id"),
    ),
    responses(
        (status = 200, description = "The turn, in whatever state it is", body = ChatbotMessageResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found (or not the caller's)", body = ErrorResponse),
    ),
)]
async fn read_message(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, mid)): Path<(String, String)>,
) -> Result<Json<ChatbotMessageResponse>, AppError> {
    let message = own_message(&id, &mid, user.get_id(), &st.db).await?;
    Ok(Json(ChatbotMessageResponse::new(&message)))
}

/// Read one turn only if the caller owns it *and* it belongs to the named
/// thread — a mismatched pair is a `404`, like a foreign one.
///
/// The thread is re-read rather than inferred from the message's `thread_id`,
/// and that is defence in depth for rows written by an older binary: a turn
/// accepted into the window of its thread's delete used to survive it, and
/// nothing sweeps such a row, so on a live volume the text of a deleted thread
/// stayed readable through these two routes. Turns can no longer be orphaned
/// (see [`ChatbotMessage`]'s insert), but the ones already there must read as
/// gone. Cheap where it sits: the SSE poll loop re-reads only the message row,
/// so this is one extra record read per polling client, not per tick of every
/// stream.
async fn own_message(
    thread: &str,
    message: &str,
    user: &UserId,
    db: &Database,
) -> Result<ChatbotMessage, AppError> {
    own_thread(thread, user, db).await?;
    ChatbotMessage::read_for(&ChatbotMessageId::from_key(message), user, db)
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
/// (cookies ride along on same-site / credentialed requests).
#[utoipa::path(
    get,
    path = "/threads/{id}/messages/{mid}/stream",
    tag = "chatbot",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "ChatbotThread id"),
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
        ChatbotMessageId::from_key(&mid),
        user.get_id().clone(),
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
            // where an abandoned stream stops polling the database, instead of
            // running on for the whole 300-second staleness window.
            tokio::select! {
                _ = ticker.tick() => {}
                _ = tx.closed() => return,
            }
            let message = match ChatbotMessage::read_for(&message_id, &user_id, &db).await {
                Ok(Some(message)) => message,
                // Deleted mid-stream (the thread went away) — say so and stop.
                Ok(None) => {
                    send_error(&tx, "not_found", "this message no longer exists").await;
                    return;
                }
                Err(err) => {
                    tracing::warn!("chat stream read failed: {err}");
                    send_error(&tx, "internal", "could not read the message").await;
                    return;
                }
            };

            match message.get_status() {
                // Includes the read-time projection: a `pending` row past its
                // staleness window arrives here as `failed`, so this loop
                // always terminates.
                MessageStatus::Pending => continue,
                MessageStatus::Complete => {
                    for chunk in slice_reply(message.get_content().as_str()) {
                        if send(&tx, "delta", json!({ "text": chunk })).await.is_err() {
                            return;
                        }
                    }
                    let done = json!({ "message": ChatbotMessageResponse::new(&message) });
                    let _ = send(&tx, "done", done).await;
                    return;
                }
                MessageStatus::Failed => {
                    let code = message.get_error_code().unwrap_or("failed");
                    send_error(&tx, code, "the assistant could not answer this message").await;
                    return;
                }
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

type EventSender = mpsc::Sender<Result<Event, axum::Error>>;

/// Queue one SSE event. `Err` means the client hung up — stop streaming.
async fn send(tx: &EventSender, name: &str, data: serde_json::Value) -> Result<(), ()> {
    let event = Event::default()
        .event(name)
        .json_data(&data)
        .unwrap_or_else(|err| {
            tracing::error!("could not encode a chat SSE event: {err}");
            Event::default()
                .event("error")
                .data("{\"code\":\"internal\"}")
        });
    tx.send(Ok(event)).await.map_err(|_| ())
}

async fn send_error(tx: &EventSender, code: &str, message: &str) {
    let _ = send(tx, "error", json!({ "code": code, "message": message })).await;
}

/// Cut a finished answer into a handful of `delta` chunks.
///
/// Fake streaming: `hab/1` is unary, so the whole text is already in hand and
/// this only lets the UI paint it progressively instead of in one jump. The
/// day the protocol grows chunk frames, this is the single function that goes
/// away — nothing else in the stream knows where a chunk came from.
///
/// Splits on character boundaries (never bytes: a clipped UTF-8 sequence would
/// render as garbage), and never returns an empty chunk. Short answers stay
/// whole — dribbling "hi" out one letter at a time is worse than not
/// pretending to stream at all.
fn slice_reply(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let size = chars.len().div_ceil(REPLY_CHUNKS).max(MIN_CHUNK_CHARS);
    chars
        .chunks(size)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slicing_preserves_the_answer_exactly() {
        for text in [
            "",
            "x",
            "kısa cevap",
            &"é".repeat(1_000),
            &"a".repeat(7_999),
        ] {
            let chunks = slice_reply(text);
            assert_eq!(
                chunks.concat(),
                text,
                "chunks must rejoin into the original answer"
            );
            assert!(
                chunks.iter().all(|chunk| !chunk.is_empty()),
                "an empty delta says nothing"
            );
            assert!(
                chunks.len() <= REPLY_CHUNKS,
                "{} chunks for {} chars",
                chunks.len(),
                text.chars().count()
            );
        }
        // Empty in, nothing out — a failed turn emits no deltas at all.
        assert!(slice_reply("").is_empty());
        // Short answers are not padded into REPLY_CHUNKS pieces.
        assert_eq!(slice_reply("hi"), vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn history_reaches_past_failed_answers_to_fill_the_window() {
        // A run of failed answers must make the window reach further back, not
        // shrink it: filtering a fixed-size tail after the fact handed the
        // service a handful of unanswered prompts and nothing older.
        let db = crate::database::init_mem().await.unwrap();
        // A real thread row: every turn is written through it, so a turn with
        // no thread is refused.
        db.query("CREATE chatbot_thread:c SET user_id = user:u, created_at = 0, updated_at = 0")
            .await
            .unwrap()
            .check()
            .unwrap();
        let thread = ChatbotThreadId::from_key("c");
        let user = UserId::from_key("u");
        let say = |text: String| ChatContent::try_new(&text).unwrap();

        for turn in 0..10 {
            ChatbotMessage::append_user(&thread, &user, say(format!("soru {turn}")), &db)
                .await
                .unwrap();
            let answer = ChatbotMessage::append_pending_assistant(&thread, &user, &db)
                .await
                .unwrap();
            ChatbotMessage::complete(answer.get_id(), say(format!("cevap {turn}")), false, &db)
                .await
                .unwrap();
        }
        // Five turns in a row whose answer never landed.
        for turn in 0..5 {
            ChatbotMessage::append_user(&thread, &user, say(format!("kayıp {turn}")), &db)
                .await
                .unwrap();
            let answer = ChatbotMessage::append_pending_assistant(&thread, &user, &db)
                .await
                .unwrap();
            ChatbotMessage::fail(answer.get_id(), "timed_out", &db)
                .await
                .unwrap();
        }
        // And this turn's own two rows, which never belong in the history.
        let prompt = ChatbotMessage::append_user(&thread, &user, say("yeni".into()), &db)
            .await
            .unwrap();
        let pending = ChatbotMessage::append_pending_assistant(&thread, &user, &db)
            .await
            .unwrap();
        let fresh = [prompt.get_id().clone(), pending.get_id().clone()];

        let history = history_for(&db, &thread, &fresh, 6).await.unwrap();
        let texts: Vec<&str> = history.iter().map(|turn| turn.content.as_str()).collect();
        assert_eq!(
            texts,
            [
                "cevap 9", "kayıp 0", "kayıp 1", "kayıp 2", "kayıp 3", "kayıp 4"
            ],
            "six settled turns, oldest first, reaching back over the failures"
        );
        // Nothing unsettled and neither of this turn's rows ever rides along.
        assert!(!texts.contains(&"yeni"));
    }

    #[tokio::test]
    async fn failure_codes_are_stable_slugs() {
        assert_eq!(
            failure_code(AiError::NoWorker("chat.reply".into())),
            "unavailable"
        );
        assert_eq!(failure_code(AiError::Timeout(30_000)), "timed_out");
        assert_eq!(
            failure_code(AiError::Remote {
                code: "quota_exhausted".into(),
                message: "no credit".into(),
            }),
            "quota_exhausted"
        );
        // A service that returns a blank code still yields something branchable.
        assert_eq!(
            failure_code(AiError::Remote {
                code: "  ".into(),
                message: String::new(),
            }),
            "service_error"
        );
    }
}
