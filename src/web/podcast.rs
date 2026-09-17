//! The podcast nest: browser ⇄ backend ⇄ the podcast service (over the QUIC
//! bridge), plus the door that hands back the produced audio.
//!
//! The relay contract is the chatbot's and the RAG nest's, shape for shape:
//! the backend owns authentication and the thin HTTP surface, the **service**
//! owns the job. Four doors map one-to-one onto the four `podcast.*`
//! capabilities — `POST /jobs` (submit), `GET /jobs/{id}` (status),
//! `GET /jobs/{id}/result` (result) and `POST /jobs/{id}/cancel` (cancel) —
//! each refusing `503` when no worker declares its capability, and each
//! relaying what the service answered: the receipt, the status snapshot, the
//! artifacts, the cancel verdict. A service-side refusal keeps its own code
//! (`not_found`, `not_ready`, `busy`, …) and decides the HTTP status, so a
//! client branches on the same vocabulary the bridge speaks.
//!
//! The fifth door is the audio itself. `podcast.result` answers an `audio_id`
//! that is a path *relative* to the school's output root on this host — the
//! service's `PODCAST_OUTPUT_ROOT`, which the deployment points at this
//! school's own directory under `FILES_PATH` — so
//! `GET /podcast/audio?path=…` streams those bytes back. It is the one place
//! a caller names a path, so it is also the one place a path is treated as
//! hostile: absolute paths, `.`/`..` segments, backslashes, drive colons and
//! symlinks that resolve out of the school's directory are all refused with a
//! `400` — "this backend will not resolve that" — never a `404` that would
//! hide the difference from the caller probing it.
//!
//! Every door is school-scoped twice: the session resolves the school, and the
//! audio door resolves the path under *that* school's directory, so a caller
//! cannot reach another school's episode even knowing its exact path.
//!
//! None of the four dispatching doors writes a row: the job record and the
//! produced files live in the service. That is also why they are synchronous —
//! every capability answers immediately (the pipeline runs in the service's own
//! pool), so there is nothing here to await off the request path.
//!
//! What the backend can and cannot judge, stated plainly: the gate is a
//! session — any authenticated member of the school may use the doors, the same
//! floor `/chatbot` and `/rag` hold — and the school wall is the bridge's
//! (`hab/2` refuses an answer that does not echo the school). It cannot scope
//! *which* sources a caller may narrate: `source_id` is the service's own
//! record, and no backend query can say what it names. That is a property of
//! the wire the two sides share, not an oversight — the service resolves the
//! id against the shared media volume and the backend enforces the tenant.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query};
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::AiBridge;
use crate::ai::podcast::{
    self, PodcastCancelPayload, PodcastCancelReply, PodcastResultPayload, PodcastResultReply,
    PodcastStatusPayload, PodcastStatusReply, PodcastSubmitPayload, PodcastSubmitReply,
};
use crate::constant::{
    AI_PODCAST_CANCEL_CAPABILITY, AI_PODCAST_RESULT_CAPABILITY, AI_PODCAST_STATUS_CAPABILITY,
    AI_PODCAST_SUBMIT_CAPABILITY,
};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;
use crate::web::tenant_state::{SchoolSlug, State};

use super::{CurrentUser, ai_unavailable};

/// How much of the audio is read per chunk, and how many chunks the body may
/// have in flight. 4 × 64 KiB is the whole memory ceiling one audio stream
/// costs, however large the file behind it.
const AUDIO_CHUNK_BYTES: usize = 64 * 1024;
const AUDIO_CHUNKS_IN_FLIGHT: usize = 4;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(submit))
        .routes(routes!(status))
        .routes(routes!(result))
        .routes(routes!(cancel))
        .routes(routes!(audio))
}

#[derive(Deserialize, ToSchema)]
struct SubmitPodcast {
    /// The backend's id for the source to narrate — a course note's id, never
    /// a path. Sent on as the service's `source_id`.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    source_id: String,
    /// Which narration to produce: `duz_okuma` (the service's default),
    /// `tek_ogretici` or `ogrenci_hoca`. The two latter need the service's LLM
    /// key and are refused with `llm_unavailable` (409) without it.
    #[schema(example = "duz_okuma")]
    #[serde(default)]
    format: Option<String>,
}

/// The service's receipt for an accepted job, passed through verbatim.
#[derive(Serialize, ToSchema)]
struct JobReceipt {
    /// The job id the service minted — every other door names it.
    job_id: String,
    /// `queued` on a fresh job.
    state: String,
    /// The service's own estimate of the job's duration, in seconds.
    eta_secs: i64,
}

/// One job's state, passed through verbatim.
#[derive(Serialize, ToSchema)]
struct JobStatus {
    job_id: String,
    /// `queued`, `running`, `done`, `failed` or `cancelled`.
    state: String,
    /// The pipeline stage the job is in.
    stage: String,
    /// Fraction complete, `0.0..=1.0`.
    progress: f64,
    /// Set only once the job failed.
    error_code: Option<String>,
}

/// A finished job's artifacts, passed through verbatim.
#[derive(Serialize, ToSchema)]
struct JobArtifacts {
    job_id: String,
    /// The produced audio, as a path relative to the school's podcast output
    /// root — feed it straight to `GET /podcast/audio?path=…`.
    #[schema(example = "ses/duz_okuma/019732e3-7b00-7000-8000-00000000dead/episode.mp3")]
    audio_id: String,
    duration_secs: f64,
    script_id: String,
    /// One entry per produced chapter; usually `[audio_id]`.
    audio_ids: Vec<String>,
    /// One entry per script the audio was aligned to.
    script_ids: Vec<String>,
    format: String,
}

/// The verdict on a cancel.
#[derive(Serialize, ToSchema)]
struct CancelVerdict {
    job_id: String,
    /// Whether *this call* cancelled something — `false` for a job that had
    /// already finished or had already been cancelled.
    cancelled: bool,
}

/// Start one podcast job. Answers `202` with the service's receipt the moment
/// the service accepts it; the pipeline then runs in the service's own worker
/// pool, so poll `GET /jobs/{id}` for progress.
#[utoipa::path(
    post,
    path = "/jobs",
    tag = "podcast",
    security(("session_cookie" = [])),
    request_body = SubmitPodcast,
    responses(
        (status = 202, description = "The job is queued; `job_id` names it from here on", body = JobReceipt),
        (status = 400, description = "Empty `source_id`, or a `format` the service does not serve", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing", body = ErrorResponse),
        (status = 503, description = "No AI service offers `podcast.submit` (or the service is at capacity)", body = ErrorResponse),
    ),
)]
async fn submit(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(_user): CurrentUser,
    Json(req): Json<SubmitPodcast>,
) -> Result<Response, AppError> {
    let bridge = match worker_for(&st, AI_PODCAST_SUBMIT_CAPABILITY) {
        Ok(bridge) => bridge,
        Err(refusal) => return Ok(refusal),
    };

    // Whitespace-only is empty: the service applies the same rule, so checking
    // it here only spares a round trip, never changes the verdict.
    let source_id = req.source_id.trim();
    if source_id.is_empty() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "source_id",
            reason: "must not be empty",
        }));
    }

    let payload = PodcastSubmitPayload {
        source_id: source_id.to_string(),
        format: req.format,
    };
    match podcast::submit(&bridge, &slug, payload).await {
        Ok(reply) => Ok((StatusCode::ACCEPTED, Json(receipt(reply))).into_response()),
        Err(code) => Ok(failure(&code)),
    }
}

/// One job's current state. Poll this; the answer is the service's own
/// snapshot.
#[utoipa::path(
    get,
    path = "/jobs/{id}",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The job as of now", body = JobStatus),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job (the service answered `not_found`)", body = ErrorResponse),
        (status = 503, description = "No AI service offers `podcast.status`", body = ErrorResponse),
    ),
)]
async fn status(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let bridge = match worker_for(&st, AI_PODCAST_STATUS_CAPABILITY) {
        Ok(bridge) => bridge,
        Err(refusal) => return Ok(refusal),
    };
    match podcast::status(&bridge, &slug, PodcastStatusPayload { job_id: id }).await {
        Ok(reply) => Ok(Json(snapshot(reply)).into_response()),
        Err(code) => Ok(failure(&code)),
    }
}

/// A finished job's artifacts — above all the `audio_id` the audio door
/// streams. A job that has not finished yet is refused by the service with
/// `not_ready` (409); one it has never heard of with `not_found` (404).
#[utoipa::path(
    get,
    path = "/jobs/{id}/result",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The finished job's artifacts", body = JobArtifacts),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job (the service answered `not_found`)", body = ErrorResponse),
        (status = 409, description = "The job has not finished (the service answered `not_ready`)", body = ErrorResponse),
        (status = 503, description = "No AI service offers `podcast.result`", body = ErrorResponse),
    ),
)]
async fn result(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let bridge = match worker_for(&st, AI_PODCAST_RESULT_CAPABILITY) {
        Ok(bridge) => bridge,
        Err(refusal) => return Ok(refusal),
    };
    match podcast::result(&bridge, &slug, PodcastResultPayload { job_id: id }).await {
        Ok(reply) => Ok(Json(artifacts(reply)).into_response()),
        Err(code) => Ok(failure(&code)),
    }
}

/// Cancel one job. `cancelled` says whether *this call* stopped work — a job
/// that had already finished, or was already cancelled, answers `false` and is
/// not an error.
#[utoipa::path(
    post,
    path = "/jobs/{id}/cancel",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The cancel verdict", body = CancelVerdict),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job (the service answered `not_found`)", body = ErrorResponse),
        (status = 503, description = "No AI service offers `podcast.cancel`", body = ErrorResponse),
    ),
)]
async fn cancel(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(_user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let bridge = match worker_for(&st, AI_PODCAST_CANCEL_CAPABILITY) {
        Ok(bridge) => bridge,
        Err(refusal) => return Ok(refusal),
    };
    match podcast::cancel(&bridge, &slug, PodcastCancelPayload { job_id: id }).await {
        Ok(reply) => Ok(Json(verdict(reply)).into_response()),
        Err(code) => Ok(failure(&code)),
    }
}

/// The query behind the audio door.
#[derive(Deserialize, IntoParams)]
struct AudioQuery {
    /// The `audio_id` from `GET /podcast/jobs/{id}/result`, **verbatim**: a
    /// path relative to this school's podcast output root.
    #[param(example = "ses/duz_okuma/019732e3-7b00-7000-8000-00000000dead/episode.mp3")]
    path: String,
}

/// Stream one produced audio file. `path` is the `audio_id`
/// `podcast.result` answered, resolved under **this caller's school** output
/// directory — the same directory the service's `PODCAST_OUTPUT_ROOT` points
/// at — and streamed in 64 KiB chunks, so a long episode costs the backend a
/// bounded buffer rather than its whole length in memory.
///
/// The path is hostile input: a leading `/`, any `.`/`..` segment, a
/// backslash, a drive colon, or a symlink whose target resolves outside the
/// school's directory is a `400` (a path this backend will not resolve), not a
/// `404` — a caller probing the boundary gets the truth, and a caller with a
/// legitimate `audio_id` never sees either.
#[utoipa::path(
    get,
    path = "/audio",
    tag = "podcast",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The audio bytes", content_type = "audio/mpeg"),
        (status = 400, description = "`path` is not a relative path inside this school's podcast output", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such file in this school's podcast output", body = ErrorResponse),
    ),
)]
async fn audio(
    State(st): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Query(query): Query<AudioQuery>,
) -> Result<Response, AppError> {
    // The school-scoped `State` has already narrowed `files_path` to the
    // caller's own directory under `FILES_PATH` — the very directory the
    // service's `PODCAST_OUTPUT_ROOT` points at — so the containment root here
    // needs no school arithmetic of its own, and cannot drift from the one
    // every other blob route uses.
    let file_path = resolve_audio(&st.files_path, &query.path).await?;
    let file = tokio::fs::File::open(&file_path)
        .await
        .map_err(|_| AppError::NotFound)?;

    Ok((
        [
            (CONTENT_TYPE, HeaderValue::from_static(audio_content_type(&file_path))),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (CACHE_CONTROL, HeaderValue::from_static("private, no-store")),
        ],
        Body::from_stream(pump(file)),
    )
        .into_response())
}

/// Resolve one `audio_id` into a real file under `root`, or refuse it.
///
/// The order is the point: shape first (so an absolute path or a `..` segment
/// never reaches the filesystem), then canonicalization — which follows every
/// symlink, so a link that only *looks* like it lives under the root resolves
/// out of it and is refused by the containment check rather than served.
async fn resolve_audio(
    root: &std::path::Path,
    relative: &str,
) -> Result<std::path::PathBuf, AppError> {
    let refused = || {
        AppError::Validation(ValidationError::Invalid {
            field: "path",
            reason: "must be a relative path inside this school's podcast output",
        })
    };

    if relative.is_empty() || relative.starts_with('/') {
        return Err(refused());
    }
    // Neither byte appears in a path this backend produced: `\` is a separator
    // on another platform, `:` is a drive designator there — and a NUL could
    // only truncate the path inside the OS. Refused here, not left to the
    // platform's rules.
    if relative.contains('\\') || relative.contains(':') || relative.contains('\0') {
        return Err(refused());
    }
    // Any `.`/`..` segment, or an empty one (`a//b`): the shape a traversal
    // needs, refused before anything is joined.
    if relative
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(refused());
    }

    // The root is canonicalized too: `FILES_PATH` may itself be reached
    // through a symlink, and containment must compare real paths.
    let root = tokio::fs::canonicalize(root)
        .await
        .map_err(|_| AppError::NotFound)?;
    let target = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|_| AppError::NotFound)?;

    // Symlink escape: `canonicalize` follows links, so a target that resolves
    // outside the school's directory lands here, whatever the link looked like.
    if !target.starts_with(&root) {
        return Err(refused());
    }
    // A directory is not an audio file — and serving `read` on it would be an
    // I/O error at best.
    if !tokio::fs::metadata(&target)
        .await
        .map(|meta| meta.is_file())
        .unwrap_or(false)
    {
        return Err(AppError::NotFound);
    }
    Ok(target)
}

/// The content type for a produced artifact, by extension. `mp3` is what the
/// pipeline writes today; the rest of the set is here so a format switch on the
/// service side still serves as audio rather than as an opaque download.
fn audio_content_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("m4a") | Some("mp4") => "audio/mp4",
        Some("ogg") | Some("oga") | Some("opus") => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// A file as a byte stream: a reader task hands chunks to the body over a
/// bounded channel, so a client that stops reading parks the pump at the next
/// send instead of pinning the whole file in memory, and a closed body ends the
/// task by itself.
fn pump(file: tokio::fs::File) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let (tx, rx) = mpsc::channel(AUDIO_CHUNKS_IN_FLIGHT);
    tokio::spawn(async move {
        let mut file = file;
        let mut buffer = vec![0u8; AUDIO_CHUNK_BYTES];
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    if tx
                        .send(Ok(Bytes::copy_from_slice(&buffer[..read])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });
    ReceiverStream::new(rx)
}

/// The 503 gate every dispatching door shares: no bridge configured, or no
/// worker declaring this door's capability. `has_capability` is documented
/// racy — fine here: it never guards a write, and the dispatch that follows
/// re-checks for real; this only spares a caller a round trip into a service
/// that cannot answer.
fn worker_for(st: &AppState, capability: &str) -> Result<AiBridge, Response> {
    let Some(bridge) = st.ai.clone() else {
        return Err(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(capability) {
        return Err(ai_unavailable("no AI service is connected right now"));
    }
    Ok(bridge)
}

/// A relayed failure as the response a client branches on: the service's own
/// code decides the status where it is one this backend knows, and anything
/// else is a `502` — the backend cannot vouch for an answer it did not
/// understand, and a gateway error is exactly that claim.
fn failure(code: &str) -> Response {
    let status = match code {
        "bad_request" => StatusCode::BAD_REQUEST,
        "not_found" => StatusCode::NOT_FOUND,
        "not_ready" | "llm_unavailable" => StatusCode::CONFLICT,
        "busy" | "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "timed_out" => StatusCode::GATEWAY_TIMEOUT,
        "internal" => StatusCode::INTERNAL_SERVER_ERROR,
        // `transport`, `protocol`, `bad_reply`, `service_error`, and any code
        // the service invented: a bad gateway.
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": code }))).into_response()
}

fn receipt(reply: PodcastSubmitReply) -> JobReceipt {
    JobReceipt {
        job_id: reply.job_id,
        state: reply.state,
        eta_secs: reply.eta_secs,
    }
}

fn snapshot(reply: PodcastStatusReply) -> JobStatus {
    JobStatus {
        job_id: reply.job_id,
        state: reply.state,
        stage: reply.stage,
        progress: reply.progress,
        error_code: reply.error_code,
    }
}

fn artifacts(reply: PodcastResultReply) -> JobArtifacts {
    JobArtifacts {
        job_id: reply.job_id,
        audio_id: reply.audio_id,
        duration_secs: reply.duration_secs,
        script_id: reply.script_id,
        audio_ids: reply.audio_ids,
        script_ids: reply.script_ids,
        format: reply.format,
    }
}

fn verdict(reply: PodcastCancelReply) -> CancelVerdict {
    CancelVerdict {
        job_id: reply.job_id,
        cancelled: reply.cancelled,
    }
}
