//! The podcast nest: browser ⇄ backend ⇄ the podcast service (over the QUIC
//! bridge), plus the doors that hand back the produced audio.
//!
//! The **backend owns the job**. `POST /podcast/jobs` mints the id, writes the
//! row, and only then dispatches `podcast.submit`; the service reports every
//! transition back (`podcast.report`) and uploads the finished mp3
//! (`BlobUploadRequest`), which lands under **this school's** blob directory.
//! `GET /podcast/jobs/{id}` and `.../result` therefore answer from
//! [`crate::db::podcast_job`] alone — no worker, no service, no second store
//! that a restart or a wiped volume could take down with it. The service's own
//! record is a mirror the pipeline works against, not the source of truth.
//!
//! Five doors:
//!
//! * `POST   /podcast/jobs`           — write the row, dispatch `podcast.submit`
//! * `GET    /podcast/jobs/{id}`      — the row's snapshot (projected, see below)
//! * `GET    /podcast/jobs/{id}/result` — a finished job's artifact references
//! * `GET    /podcast/jobs/{id}/audio`  — streams the ingested episode
//! * `POST   /podcast/jobs/{id}/cancel` — forward the cancel, stamp the verdict
//!
//! The two dispatching doors (`submit`, `cancel`) refuse `503` when no worker
//! declares their capability; the three reading doors need no service at all.
//! A relayed failure keeps the service's own code and decides the HTTP status
//! (`failure`), so a client branches on the same vocabulary the bridge speaks.
//!
//! Every door is school-scoped twice: the session resolves the school's
//! database (`st.db`) and the school's blob directory (`st.files_path`), and
//! every read is filtered by the submitting user in the statement itself — a
//! foreign id is a plain `404`, never a hint that it exists. Terminal rows
//! older than the retention window answer `410`.
//!
//! A job nobody is updating — the service died mid-pipeline — is *presented*
//! as `failed`/`interrupted` by the read path after a bounded, ETA-scaled
//! window (see [`PodcastJob::projected`]): the projection writes nothing, and
//! the service's next report overwrites it. And what the backend still cannot
//! judge is stated plainly: `source_id` is the service's own handle on shared
//! media, so no backend query can say what it names — the service resolves it,
//! the backend enforces the tenant.

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::Path;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::AiBridge;
use crate::ai::podcast::{self, PodcastCancelPayload, PodcastSubmitPayload};
use crate::constant::{
    AI_PODCAST_CANCEL_CAPABILITY, AI_PODCAST_SUBMIT_CAPABILITY, PODCAST_INTERRUPTED_CODE,
};
use crate::domain::podcast_job::{PodcastJob, PodcastJobId};
use crate::domain::user::User;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service::podcast_job as jobs;
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
        .routes(routes!(audio))
        .routes(routes!(cancel))
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

/// The receipt for an accepted job: the backend's own row plus the service's
/// ETA. `state` is always `queued` here — the row was just written.
#[derive(Serialize, ToSchema)]
struct JobReceipt {
    /// The backend-minted job id — every other door names it, and the service
    /// keys its own record by it.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    job_id: String,
    /// `queued` on a fresh job.
    state: String,
    /// The service's own estimate of the job's duration, in seconds.
    eta_secs: i64,
}

/// One job's state, from the backend's own row.
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

/// A finished job's artifacts.
#[derive(Serialize, ToSchema)]
struct JobArtifacts {
    job_id: String,
    /// The produced audio, as the blob key `GET /podcast/jobs/{id}/audio`
    /// streams — informational, never a path the caller resolves itself.
    #[schema(example = "podcast/019732e3-7b00-7000-8000-00000000dead.mp3")]
    audio_id: String,
    duration_secs: Option<f64>,
    /// The resolved narration format.
    format: Option<String>,
}

/// The verdict on a cancel.
#[derive(Serialize, ToSchema)]
struct CancelVerdict {
    job_id: String,
    /// Whether *this call* cancelled something — `false` for a job that had
    /// already finished or had already been cancelled.
    cancelled: bool,
}

/// Start one podcast job. Answers `202` with the backend's own receipt the
/// moment the service accepts it.
///
/// The row is written **before** the dispatch, so a poll that races the submit
/// finds the job rather than a `404`; and every dispatch outcome that is not an
/// acceptance leaves the row `failed` with a reason code rather than claiming
/// `queued` forever. With no worker connected the row is not written at all —
/// the `503` is the whole answer.
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
    CurrentUser(user): CurrentUser,
    Json(req): Json<SubmitPodcast>,
) -> Result<Response, AppError> {
    let bridge = match worker_for(&st, AI_PODCAST_SUBMIT_CAPABILITY) {
        Ok(bridge) => bridge,
        Err(no_worker) => return Ok(no_worker.refusal()),
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

    let job = jobs::create(&st.db, user.get_id(), source_id, req.format.as_deref()).await?;
    let job_id = job.get_id().key();
    let payload = PodcastSubmitPayload {
        job_id: job_id.clone(),
        source_id: source_id.to_string(),
        format: req.format.clone(),
        user_id: user.get_id().key(),
    };
    match podcast::submit(&bridge, &slug, payload).await {
        // The echo is checked, not trusted: this id is the only handle every
        // later call uses, so an answer about some other job is a protocol
        // failure, not a receipt.
        Ok(reply) if reply.job_id == job_id => {
            let eta_secs = reply.eta_secs;
            jobs::set_eta(&st.db, job.get_id(), eta_secs).await?;
            Ok((
                StatusCode::ACCEPTED,
                Json(JobReceipt {
                    job_id,
                    state: reply.state,
                    eta_secs,
                }),
            )
                .into_response())
        }
        Ok(reply) => {
            tracing::warn!(
                "AI podcast service accepted job {} under the id {}",
                job_id,
                reply.job_id
            );
            jobs::mark_failed(&st.db, job.get_id(), "bad_reply").await?;
            Ok(failure("bad_reply"))
        }
        // The service refused the job: the row is the record of that, and the
        // refusal reaches the caller with its own status.
        Err(code) => {
            jobs::mark_failed(&st.db, job.get_id(), &code).await?;
            Ok(failure(&code))
        }
    }
}

/// One job's current state — the backend's own row, so this door answers with
/// the service down, restarted, or never connected at all. A job nobody has
/// updated inside its ETA-scaled window reads as `failed`/`interrupted`; that
/// projection is read-side only and never written.
#[utoipa::path(
    get,
    path = "/jobs/{id}",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The job as of now", body = JobStatus),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job for this caller (unknown id, or another user's)", body = ErrorResponse),
        (status = 410, description = "The job is past its retention window", body = ErrorResponse),
    ),
)]
async fn status(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let job = owned(&st, &user, &id).await?;
    Ok(Json(JobStatus {
        job_id: job.get_id().key(),
        state: job.get_state().as_str().to_string(),
        stage: job.get_stage().to_string(),
        progress: job.get_progress(),
        error_code: job.get_error_code().map(str::to_string),
    })
    .into_response())
}

/// A finished job's artifact references. A job that has not finished is a
/// `409 not_ready`; one the caller does not own, a `404`.
#[utoipa::path(
    get,
    path = "/jobs/{id}/result",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The finished job's artifacts", body = JobArtifacts),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job for this caller", body = ErrorResponse),
        (status = 409, description = "The job has not finished (`code: not_ready`)", body = ErrorResponse),
        (status = 410, description = "The job is past its retention window", body = ErrorResponse),
    ),
)]
async fn result(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let job = owned(&st, &user, &id).await?;
    let audio_id = match job.get_state() {
        crate::domain::podcast_job::PodcastJobState::Done => job
            .get_audio_key()
            .expect("a done row always carries its audio (schema CHECK)")
            .to_string(),
        _ => {
            return Err(AppError::ConflictCoded {
                code: "not_ready",
                message: format!("job `{}` is {}", job.get_id().key(), job.get_state().as_str()),
            });
        }
    };
    Ok(Json(JobArtifacts {
        job_id: job.get_id().key(),
        audio_id,
        duration_secs: job.get_duration_secs(),
        format: job.get_format().map(str::to_string),
    })
    .into_response())
}

/// Stream one produced episode. The bytes are this school's own — ingested
/// under this school's blob directory — and the content type is the one the
/// upload declared, replayed from the row.
///
/// A job that is not `done` is a `409 not_ready`; a row whose blob is missing
/// from the host is a `409 audio_missing` (the row is intact, the bytes are
/// not — a state a client can report, unlike a generic `500`).
#[utoipa::path(
    get,
    path = "/jobs/{id}/audio",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The audio bytes", content_type = "audio/mpeg"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job for this caller", body = ErrorResponse),
        (status = 409, description = "Not finished yet (`not_ready`), or its blob is missing from this host (`audio_missing`)", body = ErrorResponse),
        (status = 410, description = "The job is past its retention window", body = ErrorResponse),
    ),
)]
async fn audio(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let job = owned(&st, &user, &id).await?;
    let key = match job.get_state() {
        crate::domain::podcast_job::PodcastJobState::Done => job
            .get_audio_key()
            .expect("a done row always carries its audio (schema CHECK)")
            .to_string(),
        _ => {
            return Err(AppError::ConflictCoded {
                code: "not_ready",
                message: format!("job `{}` is {}", job.get_id().key(), job.get_state().as_str()),
            });
        }
    };
    let path = crate::web::blob_path(&st.files_path, &key);
    let file = tokio::fs::File::open(&path).await.map_err(|err| {
        tracing::error!("missing blob for podcast job {}: {err}", job.get_id().key());
        AppError::ConflictCoded {
            code: "audio_missing",
            message: "the episode's file is missing on this host".to_string(),
        }
    })?;
    let content_type = job.get_audio_type().unwrap_or("application/octet-stream");

    Ok((
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_str(content_type)
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            ),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (CACHE_CONTROL, HeaderValue::from_static("private, no-store")),
        ],
        Body::from_stream(pump(file)),
    )
        .into_response())
}

/// Cancel one job. `cancelled` says whether *this call* stopped work — a job
/// that had already finished, or was already cancelled, answers `false` and is
/// not an error. Only a live job needs the service; a terminal one is answered
/// from the row without a worker. A service that has never heard of a live job
/// (its store was wiped) leaves the row `failed`/`interrupted`: the job cannot
/// finish, and the row says so.
#[utoipa::path(
    post,
    path = "/jobs/{id}/cancel",
    tag = "podcast",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Job id from `POST /podcast/jobs`")),
    responses(
        (status = 200, description = "The cancel verdict", body = CancelVerdict),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "No such job for this caller", body = ErrorResponse),
        (status = 410, description = "The job is past its retention window", body = ErrorResponse),
        (status = 503, description = "The job is live but no AI service offers `podcast.cancel`", body = ErrorResponse),
    ),
)]
async fn cancel(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let job = owned(&st, &user, &id).await?;
    let job_id = job.get_id().key();
    if job.get_state().is_terminal() {
        return Ok(Json(CancelVerdict {
            job_id,
            cancelled: false,
        })
        .into_response());
    }
    let bridge = match worker_for(&st, AI_PODCAST_CANCEL_CAPABILITY) {
        Ok(bridge) => bridge,
        Err(no_worker) => return Ok(no_worker.refusal()),
    };
    let verdict = |cancelled: bool| {
        Json(CancelVerdict {
            job_id: job.get_id().key(),
            cancelled,
        })
        .into_response()
    };
    match podcast::cancel(
        &bridge,
        &slug,
        PodcastCancelPayload {
            job_id: job_id.clone(),
        },
    )
    .await
    {
        Ok(reply) if reply.cancelled => {
            jobs::mark_cancelled(&st.db, job.get_id()).await?;
            Ok(verdict(true))
        }
        // Not cancelled by this call: either it finished first (its terminal
        // report is on its way) or it was already cancelled.
        Ok(_) => Ok(verdict(false)),
        // The service has never heard of a job the row calls live — its store
        // was wiped or rolled back. The row records the truth: it cannot finish.
        Err(code) if code == "not_found" => {
            tracing::warn!("AI podcast service lost live job {job_id}");
            jobs::mark_failed(&st.db, job.get_id(), PODCAST_INTERRUPTED_CODE).await?;
            Ok(verdict(false))
        }
        Err(code) => Ok(failure(&code)),
    }
}

/// Read one job for this caller, projected and expiry-checked — the three
/// reading doors' shared first act. A foreign id and an unknown id are the
/// same `404`; a job past its retention window is a `410`.
async fn owned(st: &AppState, user: &User, id: &str) -> Result<PodcastJob, AppError> {
    let job = jobs::read_for(&st.db, &PodcastJobId::from_key(id), user.get_id())
        .await?
        .ok_or(AppError::NotFound)?
        .projected();
    if job.is_expired() {
        return Err(AppError::Expired("this podcast job has expired"));
    }
    Ok(job)
}

/// A file as a byte stream: a reader task hands chunks to the body over a
/// bounded channel, so a client that stops reading parks the pump at the next
/// send instead of pinning the whole file in memory, and a closed body ends the
/// task by itself.
fn pump(file: tokio::fs::File) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(AUDIO_CHUNKS_IN_FLIGHT);
    tokio::spawn(async move {
        let mut file = file;
        let mut buf = vec![0u8; AUDIO_CHUNK_BYTES];
        loop {
            match tokio::io::AsyncReadExt::read(&mut file, &mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ok(Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
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

/// Why a dispatching door has no worker to talk to. Deliberately small: the
/// refusal is not a response — the caller builds the `503` at its own call
/// site, so a large, rarely-taken `Response` never rides the `Result` type of
/// a hot path.
enum NoWorker {
    /// No AI service is configured on this deployment at all.
    Disabled,
    /// A bridge is configured, but no connected worker declares this door's
    /// capability.
    Unconnected,
}

impl NoWorker {
    /// The `503` the caller returns for this refusal.
    fn refusal(self) -> Response {
        ai_unavailable(match self {
            NoWorker::Disabled => "the AI service is not enabled on this deployment",
            NoWorker::Unconnected => "no AI service is connected right now",
        })
    }
}

/// The 503 gate the two dispatching doors share: no bridge configured, or no
/// worker declaring this door's capability. `has_capability` is documented
/// racy — fine here: it never guards a write, and the dispatch that follows
/// re-checks for real; this only spares a caller a round trip into a service
/// that cannot answer.
fn worker_for(st: &AppState, capability: &str) -> Result<AiBridge, NoWorker> {
    let Some(bridge) = st.ai.clone() else {
        return Err(NoWorker::Disabled);
    };
    if !bridge.has_capability(capability) {
        return Err(NoWorker::Unconnected);
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
