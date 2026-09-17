//! Payload contract for the `podcast.*` capabilities, and the dispatch behind
//! them.
//!
//! The podcast service turns one stored source — a course note's PDF — into an
//! audio episode, and that is a *job*: submit enqueues it, status and result
//! read it back minutes later, cancel stops it. So these are four verbs on one
//! record that the **service** owns, not four steps of one backend request,
//! and the backend relays each call to the worker declaring the matching
//! capability.
//!
//! Nothing here is spawned, unlike [`chat`](crate::ai::chat) and
//! [`rag_chat`](crate::ai::rag_chat): every one of the four calls answers
//! immediately — a queued receipt, a status snapshot, a finished job, a cancel
//! verdict — while the pipeline itself runs in the service's own worker pool.
//! That is also why the four ride the bridge's ordinary request deadline
//! ([`AI_DEFAULT_REQUEST_TIMEOUT_SECS`](crate::constant::AI_DEFAULT_REQUEST_TIMEOUT_SECS))
//! rather than a per-capability one: no call waits on a model.
//!
//! Tenancy is the transport's job: [`crate::ai::server`] stamps the caller's
//! school onto every `hab/2` request and refuses an answer that does not echo
//! it, so a worker registered by one school never answers another's call —
//! the wall holds even though the job records themselves live outside the
//! backend's databases.
//!
//! The produced audio does not cross the bridge. `podcast.result` answers an
//! `audio_id` that is a path *relative* to the service's
//! `PODCAST_OUTPUT_ROOT`, which the deployment points at the school's own
//! files directory on this host (`FILES_PATH/<slug>`, the same volume every
//! upload lands in); the browser then fetches it from `GET /podcast/audio`,
//! which resolves it under that directory and refuses everything that would
//! escape it — see [`crate::web::podcast`].

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ai::{AiBridge, AiError};
use crate::tenant::Slug;

/// The capability strings, re-exported so a reader of the payload contract
/// finds them next to the payloads — the same shape as
/// [`crate::ai::rag`](crate::ai::rag).
pub use crate::constant::{
    AI_PODCAST_CANCEL_CAPABILITY, AI_PODCAST_RESULT_CAPABILITY, AI_PODCAST_STATUS_CAPABILITY,
    AI_PODCAST_SUBMIT_CAPABILITY,
};

/// What the backend asks a podcast service to start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastSubmitPayload {
    /// The backend's record id for the source to narrate — a course note id,
    /// never a file path. The service resolves it against its own media root,
    /// which the deployment points at this backend's files volume.
    pub source_id: String,
    /// Which narration to produce. `duz_okuma` (plain reading) is the
    /// service's default and the only one that needs no LLM key;
    /// `tek_ogretici` and `ogrenci_hoca` are refused by the service with
    /// `llm_unavailable` when the key is missing. Sent verbatim — the
    /// service's closed set is what validates it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

/// The receipt for an accepted job, echoed straight to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastSubmitReply {
    /// The id the service minted for the job. Every later call names it.
    pub job_id: String,
    /// The job's state right after submission (`queued`).
    pub state: String,
    /// The service's own estimate of how long the job will take, in seconds.
    pub eta_secs: i64,
}

/// What the backend asks about one job's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastStatusPayload {
    pub job_id: String,
}

/// One job's state, as of now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodcastStatusReply {
    pub job_id: String,
    /// `queued`, `running`, `done`, `failed` or `cancelled` — the service's own
    /// vocabulary, passed through rather than mapped, so a state added there
    /// needs no change here.
    pub state: String,
    /// The pipeline stage the job is in, as prose from the service.
    pub stage: String,
    /// Fraction complete, `0.0..=1.0`.
    pub progress: f64,
    /// The failure code when `state` is `failed`, null otherwise.
    #[serde(default)]
    pub error_code: Option<String>,
}

/// What the backend asks for a finished job's artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastResultPayload {
    pub job_id: String,
}

/// A finished job's artifacts. Only a `done` job answers this: the service
/// refuses any other state with `not_ready`, so nothing here is nullable — a
/// reply missing `audio_id` is an unreadable answer, not a half-finished job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodcastResultReply {
    pub job_id: String,
    /// The produced audio, as a path **relative** to `PODCAST_OUTPUT_ROOT` on
    /// this host. Never absolute, never containing `..` — and
    /// [`crate::web::podcast`] refuses one that is, whatever the service sent.
    pub audio_id: String,
    /// The audio's length in seconds.
    pub duration_secs: f64,
    /// The script the narration was read from.
    pub script_id: String,
    /// One entry per produced chapter, when the job was sliced into several;
    /// usually `[audio_id]`.
    #[serde(default)]
    pub audio_ids: Vec<String>,
    /// One entry per script the audio was aligned to.
    #[serde(default)]
    pub script_ids: Vec<String>,
    /// The narration format the job was produced with.
    pub format: String,
}

/// What the backend asks to cancel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastCancelPayload {
    pub job_id: String,
}

/// The verdict on a cancel: `cancelled` is "this call cancelled something",
/// not "the job is cancelled" — cancelling an already-finished job answers
/// `false`, and so does cancelling one that was already cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastCancelReply {
    pub job_id: String,
    pub cancelled: bool,
}

/// Start one job. `Err` is a short, stable code — see [`failure_code`].
pub async fn submit(
    bridge: &AiBridge,
    slug: &Slug,
    payload: PodcastSubmitPayload,
) -> Result<PodcastSubmitReply, String> {
    dispatch(bridge, slug, AI_PODCAST_SUBMIT_CAPABILITY, payload).await
}

/// Read one job's progress.
pub async fn status(
    bridge: &AiBridge,
    slug: &Slug,
    payload: PodcastStatusPayload,
) -> Result<PodcastStatusReply, String> {
    dispatch(bridge, slug, AI_PODCAST_STATUS_CAPABILITY, payload).await
}

/// Read one finished job's artifacts.
pub async fn result(
    bridge: &AiBridge,
    slug: &Slug,
    payload: PodcastResultPayload,
) -> Result<PodcastResultReply, String> {
    dispatch(bridge, slug, AI_PODCAST_RESULT_CAPABILITY, payload).await
}

/// Cancel one job.
pub async fn cancel(
    bridge: &AiBridge,
    slug: &Slug,
    payload: PodcastCancelPayload,
) -> Result<PodcastCancelReply, String> {
    dispatch(bridge, slug, AI_PODCAST_CANCEL_CAPABILITY, payload).await
}

/// One `podcast.*` round trip: encode, dispatch, decode. Every capability
/// travels this path, so the encode/decode failure vocabulary is written once.
///
/// The reply is decoded into the *typed* contract rather than handed back as
/// [`serde_json::Value`]: the service is a trust boundary, and a field the
/// backend then acts on (above all `audio_id`, which becomes a path under a
/// school's files directory) must exist and be a string before anything uses
/// it.
async fn dispatch<P, R>(
    bridge: &AiBridge,
    slug: &Slug,
    capability: &str,
    payload: P,
) -> Result<R, String>
where
    P: Serialize,
    R: DeserializeOwned,
{
    let payload = serde_json::to_value(payload).map_err(|err| {
        tracing::error!("could not encode a {capability} request: {err}");
        "internal".to_string()
    })?;

    let raw = bridge
        .dispatch(slug, capability, payload)
        .await
        .map_err(failure_code)?;

    serde_json::from_value(raw).map_err(|err| {
        tracing::warn!("AI service answered with an unreadable {capability} payload: {err}");
        "bad_reply".to_string()
    })
}

/// A dispatch failure as a short, stable code the web layer maps to a status.
/// Mirrors `web::chatbot`'s and `ai::rag_chat`'s helpers.
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
            tracing::warn!("AI podcast service refused the request: {code}: {message}");
            if code.trim().is_empty() {
                "service_error".to_string()
            } else {
                code
            }
        }
    }
}
