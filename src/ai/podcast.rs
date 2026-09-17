//! Payload contract for the `podcast.*` capabilities and the audio ingest —
//! both directions of the podcast seam.
//!
//! The podcast service turns one stored source — a course note's PDF — into an
//! audio episode. Since 2026-09-17 the **backend owns that job's record**: the
//! submit door mints the job id, writes the row, and then dispatches
//! `podcast.submit` with that id; the service reports every transition back
//! through the client-initiated `podcast.report` capability, and uploads the
//! finished mp3 as raw bytes on a `BlobUploadRequest` stream. Status and
//! result answer from the backend's own row — they are not capabilities any
//! more, because a service restart (or a wiped service volume) must cost
//! nothing but the liveness the read-side projection already covers.
//!
//! What remains **server-initiated** are the two calls that must reach a live
//! worker:
//!
//! * [`submit`] — `podcast.submit`, whose payload carries the backend-minted
//!   `job_id` and the submitting `user_id` (the service stores both; its own
//!   record is keyed by the id the backend will keep asking about).
//! * [`cancel`] — `podcast.cancel`, unchanged: a write on the service's queue,
//!   and the capability is what decides whether a deployment offers the door.
//!
//! Tenancy is the transport's job: [`crate::ai::server`] stamps the caller's
//! school onto every `hab/2` request and resolves the frame's school on every
//! client-initiated one; this module never re-derives a school.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::AiBridge;
use crate::ai::error::AiError;
use crate::database::Database;
use crate::domain::podcast_job::PodcastJobId;
use crate::service::podcast_job::{PodcastRefusal, ReportInput};
use crate::tenant::Slug;

/// The capability strings, re-exported so a reader of the payload contract
/// finds them next to the payloads — the same shape as
/// [`crate::ai::rag`](crate::ai::rag).
pub use crate::constant::{
    AI_PODCAST_CANCEL_CAPABILITY, AI_PODCAST_REPORT_CAPABILITY, AI_PODCAST_SUBMIT_CAPABILITY,
};

/// What the backend asks a podcast service to start. The `job_id` is the
/// backend's own minted id — the service keys its record by it and echoes it
/// in every report, so the two stores can never disagree about which job a
/// record is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastSubmitPayload {
    pub job_id: String,
    pub source_id: String,
    /// `None` means the service applies its own default (`duz_okuma` today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The submitting user, so the service's own record carries the same
    /// owner the backend row does.
    pub user_id: String,
}

/// The receipt for an accepted job. `job_id` must echo the request's — the
/// backend checks, because the id is the only handle it will keep polling by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastSubmitReply {
    pub job_id: String,
    /// `queued` on a fresh job.
    pub state: String,
    /// The service's own estimate of the job's duration, in seconds.
    pub eta_secs: i64,
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

/// One state report from the service — the payload of a client-initiated
/// [`AI_PODCAST_REPORT_CAPABILITY`] call. The service sends one per
/// transition, in order (it awaits each answer before sending the next), and
/// echoes the job's own identity so the backend can refuse a report that
/// describes a different job than the row holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PodcastReportPayload {
    pub job_id: String,
    pub source_id: String,
    #[serde(default)]
    pub format: Option<String>,
    pub user_id: String,
    /// `queued` | `running` | `done` | `failed` | `cancelled`.
    pub state: String,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub error_code: Option<String>,
}

/// The answer to one report. `stored` is always `true` on an `Ok` — an
/// unstored report is a refusal, never a quiet success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodcastReportReply {
    pub job_id: String,
    pub stored: bool,
}

/// Start one job. `Err` is a short, stable code — see [`failure_code`].
pub async fn submit(
    bridge: &AiBridge,
    slug: &Slug,
    payload: PodcastSubmitPayload,
) -> Result<PodcastSubmitReply, String> {
    dispatch(bridge, slug, AI_PODCAST_SUBMIT_CAPABILITY, payload).await
}

/// Cancel one job.
pub async fn cancel(
    bridge: &AiBridge,
    slug: &Slug,
    payload: PodcastCancelPayload,
) -> Result<PodcastCancelReply, String> {
    dispatch(bridge, slug, AI_PODCAST_CANCEL_CAPABILITY, payload).await
}

/// Handle one client-initiated `podcast.report` call: decode the payload,
/// hand it to the workflow layer (which owns every check), and answer the
/// reply payload — or the flat `(code, message)` refusal the bridge frames.
///
/// The decode is deliberately strict: a payload that does not fit
/// [`PodcastReportPayload`] is `invalid_payload`, never coerced into a
/// half-filled report.
pub async fn report(db: &Database, payload: Value) -> Result<Value, (&'static str, String)> {
    let report: PodcastReportPayload = serde_json::from_value(payload).map_err(|err| {
        (
            "invalid_payload",
            format!("the report payload does not fit the contract: {err}"),
        )
    })?;
    let input = ReportInput {
        job_id: &report.job_id,
        source_id: &report.source_id,
        format: report.format.as_deref(),
        user_id: &report.user_id,
        state: &report.state,
        stage: &report.stage,
        progress: report.progress,
        error_code: report.error_code.as_deref(),
    };
    match crate::service::podcast_job::report(db, &input).await {
        Ok(job) => Ok(serde_json::to_value(PodcastReportReply {
            job_id: job.get_id().key(),
            stored: true,
        })
        .expect("a report reply is always encodable")),
        Err(refusal) => Err((refusal.code(), refusal.message())),
    }
}

/// A [`PodcastRefusal`] as the capability frame's flat `(code, message)`.
pub fn refusal(refusal: &PodcastRefusal) -> (&'static str, String) {
    (refusal.code(), refusal.message())
}

/// Ingest one uploaded episode: validate the frame's metadata, check the job,
/// read exactly `size` bytes into a temp file under the school's blob root,
/// rename it into place, and stamp the row.
///
/// `files_root` is **the school's own** directory (the caller resolves the
/// slug); the file lands under its `podcast/` subdirectory, keyed by the job
/// id, which is what the job row's own `audio_key` then publishes.
///
/// The order is the point, and the same one the HTTP doors hold: a refusal
/// must cost the sender nothing, so metadata and the job's existence are
/// checked before a byte is read; the bytes are complete on disk (renamed,
/// never half-written) before the row names them; and a row write that finds
/// the job gone deletes the file it just stored rather than leaving an orphan.
pub async fn ingest<R>(
    db: &Database,
    files_root: &std::path::Path,
    request: &crate::ai::protocol::BlobUploadRequest,
    body: &mut R,
) -> Result<(String, u64), PodcastRefusal>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncWriteExt as _;

    crate::service::podcast_job::validate_audio(
        &request.name,
        &request.content_type,
        request.size,
    )?;
    let id = PodcastJobId::from_key(&request.job_id);
    let Some(job) = crate::db::podcast_job::read(db, &id).await? else {
        return Err(PodcastRefusal::UnknownJob);
    };
    if job.is_expired() {
        return Err(PodcastRefusal::Expired);
    }

    let key = crate::service::podcast_job::audio_key(&id, &request.name, &request.content_type);
    let dir = files_root.join("podcast");
    tokio::fs::create_dir_all(&dir).await.map_err(|err| {
        PodcastRefusal::Unavailable(format!("could not create the podcast blob directory: {err}"))
    })?;
    let temp = dir.join(format!(".upload-{}.tmp", crate::domain::monotonic_id::next_uuid()));
    let mut file = tokio::fs::File::create(&temp).await.map_err(|err| {
        PodcastRefusal::Unavailable(format!("could not open the upload's temp file: {err}"))
    })?;

    let mut remaining = request.size;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let read = match tokio::io::AsyncReadExt::read(body, &mut buf[..want]).await {
            Ok(0) => {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(PodcastRefusal::InvalidPayload(
                    "the stream ended before `size` bytes arrived".to_string(),
                ));
            }
            Ok(n) => n,
            Err(err) => {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(PodcastRefusal::Unavailable(format!(
                    "reading the upload failed: {err}"
                )));
            }
        };
        if let Err(err) = file.write_all(&buf[..read]).await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(PodcastRefusal::Unavailable(format!(
                "writing the upload failed: {err}"
            )));
        }
        remaining -= read as u64;
    }
    file.flush()
        .await
        .map_err(|err| PodcastRefusal::Unavailable(format!("flushing the upload failed: {err}")))?;
    drop(file);
    let final_path = files_root.join(&key);
    tokio::fs::rename(&temp, &final_path).await.map_err(|err| {
        PodcastRefusal::Unavailable(format!("could not move the upload into place: {err}"))
    })?;

    match crate::service::podcast_job::set_audio(
        db,
        &id,
        &key,
        &request.name,
        &request.content_type,
        request.size as i64,
        request.duration_secs,
    )
    .await
    {
        Ok(Some(_)) => Ok((key, request.size)),
        // The job vanished between the check above and the row write: take the
        // bytes back out rather than leaving a file nothing names.
        Ok(None) => {
            let _ = tokio::fs::remove_file(&final_path).await;
            Err(PodcastRefusal::UnknownJob)
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&final_path).await;
            Err(err.into())
        }
    }
}

/// One `podcast.*` round trip: encode, dispatch, decode. Every capability
/// travels this path, so the encode/decode failure vocabulary is written once.
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
