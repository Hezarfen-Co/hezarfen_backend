//! The QUIC listener AI services dial into, and the request path out to them.
//!
//! See [`super::protocol`] for the wire shape. The backend listens rather than
//! dials so a service can sit behind NAT, restart without the backend needing
//! to know its address, and scale out by opening a second connection.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use serde_json::Value;
use tower::ServiceExt;
use tracing::Instrument;
use ulid::Ulid;

use crate::ai::error::AiError;
use crate::ai::protocol::{
    ApiRequest, ApiResponse, BlobRequest, BlobResponse, FrameError, Greeting, Hello, RejectCode,
    Request, Response, protocol_matches, read_frame, write_frame,
};
use crate::ai::registry::{AiRegistry, WorkerSnapshot, clamp_concurrency};
use crate::ai::tls;
use crate::constant::{
    AI_BLOB_WRITE_STALL_SECS, AI_HANDSHAKE_TIMEOUT_SECS, AI_MAX_FRAME_BYTES, AI_PROTOCOL,
    REQUEST_TIMEOUT_SECS,
};
use crate::database::Database;

use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::user::{User, UserId};
use crate::error::AppError;
use crate::module::Module;
use crate::state::DbHealth;
use crate::telemetry::Metrics;
use crate::tenant::{Slug, Tenants};
use crate::web::blob_path;
use crate::web::courses::can_view_course;
use crate::web::extractor::AiPrincipal;
use crate::web::tenant_state::{ResolvedTenant, TenantExt, school_files_path};

/// What the bridge needs to come up.
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    /// UDP address to listen on (`AI_QUIC_ADDR`).
    pub addr: SocketAddr,
    /// Shared secret every service must present (`AI_SHARED_TOKEN`).
    pub token: String,
    /// PEM paths; both absent means a self-signed certificate is generated.
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
    /// How long one dispatch may take (`AI_REQUEST_TIMEOUT_SECS`).
    pub request_timeout: Duration,
}

/// What an api read needs to be answered: the router to dispatch into, the
/// registry every school is resolved through, and the liveness flag that
/// stands in for the HTTP db guard this path bypasses.
///
/// Deployment-wide, not per-school: `hab/2` frames name their own school, so
/// one bridge (and one AI fleet) serves every school here.
struct ApiHandle {
    router: axum::Router,
    tenants: Tenants,
    db_up: DbHealth,
    /// Where uploaded blobs live — the deployment root; a school's own
    /// directory is [`school_files_path`] of it.
    files_path: std::path::PathBuf,
}

impl ApiHandle {
    /// Resolve the slug a frame named into that school's database, or the
    /// refusal the service gets.
    ///
    /// The three codes are distinct on purpose: `malformed` is the service's
    /// own bug (that string is no slug), `unknown_school` means the deployment
    /// has no such customer, and `school_suspended` means it has one that is
    /// switched off — a service that retries the first two forever learns
    /// nothing, while the third is worth retrying later.
    async fn school(&self, school: &str) -> Result<ResolvedTenant, (&'static str, String)> {
        let slug = Slug::try_new(school).map_err(|err| {
            (
                "malformed",
                format!("`{school}` is not a school slug: {err}"),
            )
        })?;
        self.tenants.resolve(&slug).await.map_err(|err| match err {
            AppError::Unauthorized => (
                "unknown_school",
                format!("this deployment serves no `{slug}` school"),
            ),
            AppError::Forbidden(_) => (
                "school_suspended",
                format!("the `{slug}` school is suspended"),
            ),
            other => (
                "unavailable",
                format!("the `{slug}` school is not reachable: {other}"),
            ),
        })
    }

    /// One school's blob directory.
    fn files_dir(&self, slug: &Slug) -> std::path::PathBuf {
        school_files_path(&self.files_path, slug)
    }
}

struct Inner {
    registry: AiRegistry,
    /// Armed once per process by [`crate::build_router`] (see
    /// [`AiBridge::arm_api`]). A `OnceLock` because the router is built after the
    /// listener is bound and never replaced afterwards: writing it costs one
    /// store at boot and reading it is lock-free on every api read.
    api: OnceLock<ApiHandle>,
    /// The leaf certificate this listener presents, kept so it can be shown to
    /// an operator (or pinned by an in-process client) without re-reading the
    /// PEM off disk.
    certificate: rustls::pki_types::CertificateDer<'static>,
    token: String,
    request_timeout: Duration,
    endpoint: quinn::Endpoint,
    /// Filled by [`AiBridge::arm_api`], for the same reason `api` is: the
    /// process's instruments are built with the router, after the listener is
    /// bound.
    metrics: OnceLock<Metrics>,
}

impl Inner {
    /// The instruments to record into. A service that dials in during the boot
    /// window gets the noop set, which — with no OTLP endpoint configured — is
    /// exactly what the armed one is anyway.
    fn metrics(&self) -> &Metrics {
        self.metrics.get_or_init(Metrics::noop)
    }

    /// How many services are connected right now, as the gauge sees it.
    fn record_worker_count(&self) {
        self.metrics()
            .ai_workers
            .record(self.registry.snapshot().len() as u64, &[]);
    }
}

/// Count one refused handshake. `reason` comes from a fixed vocabulary —
/// never a message — so the metric's label cardinality stays bounded.
fn handshake_failed(metrics: &Metrics, reason: &'static str) {
    metrics
        .ai_handshake_failures_total
        .add(1, &[KeyValue::new("reason", reason)]);
}

/// Handle to a running bridge. Cheap to clone; every clone shares one listener
/// and one registry.
#[derive(Clone)]
pub struct AiBridge {
    inner: Arc<Inner>,
}

impl AiBridge {
    /// Bind the listener and start accepting services in the background.
    ///
    /// Returns as soon as the socket is bound — services are expected to dial
    /// in on their own schedule, so the backend never waits for one. A request
    /// issued before any service connects fails with [`AiError::NoWorker`]
    /// rather than blocking, which is what keeps an AI feature's outage
    /// confined to that feature.
    pub async fn bind(config: BridgeConfig) -> Result<Self, AiError> {
        if config.token.trim().is_empty() {
            return Err(AiError::Setup(
                "AI_SHARED_TOKEN must be set when the AI bridge is enabled".into(),
            ));
        }
        let bridge_tls = tls::build(config.cert_path.as_deref(), config.key_path.as_deref())?;
        let fingerprint = tls::fingerprint(&bridge_tls.leaf);
        let endpoint =
            quinn::Endpoint::server(bridge_tls.server_config, config.addr).map_err(|e| {
                AiError::Setup(format!("cannot bind the AI bridge to {}: {e}", config.addr))
            })?;
        let bound = endpoint
            .local_addr()
            .map_err(|e| AiError::Setup(format!("bound socket has no address: {e}")))?;

        let inner = Arc::new(Inner {
            registry: AiRegistry::default(),
            api: OnceLock::new(),
            certificate: bridge_tls.leaf.clone(),
            token: config.token,
            request_timeout: config.request_timeout,
            endpoint,
            metrics: OnceLock::new(),
        });
        tracing::info!(
            "AI bridge listening on {bound} (protocol {AI_PROTOCOL}, cert sha256 {fingerprint})"
        );
        tokio::spawn(accept_loop(Arc::clone(&inner)));
        Ok(Self { inner })
    }

    /// The address actually bound. Differs from the configured one when port 0
    /// was requested (as the tests do).
    pub fn local_addr(&self) -> Result<SocketAddr, AiError> {
        self.inner
            .endpoint
            .local_addr()
            .map_err(|e| AiError::Transport(e.to_string()))
    }

    /// The listener's leaf certificate, DER-encoded. A service pins this (or
    /// its [`AiBridge::certificate_fingerprint`]) instead of installing a CA.
    pub fn certificate(&self) -> rustls::pki_types::CertificateDer<'static> {
        self.inner.certificate.clone()
    }

    /// SHA-256 of [`AiBridge::certificate`], hex — the value logged at boot.
    pub fn certificate_fingerprint(&self) -> String {
        tls::fingerprint(&self.inner.certificate)
    }

    /// Every connected service, for logs and a future health endpoint.
    pub fn workers(&self) -> Vec<WorkerSnapshot> {
        self.inner.registry.snapshot()
    }

    /// Is anything connected that can serve `capability`?
    ///
    /// This registry owns the sockets, so it is the whole truth about what the
    /// backend can answer. Racy in the harmless direction — a worker can vanish
    /// between the check and the dispatch — so it never guards a write.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.inner.registry.has_capability(capability)
    }

    /// Send one request to a service and await its answer.
    ///
    /// Each call takes a fresh QUIC bidirectional stream, so concurrent calls
    /// over the same connection neither block nor interleave: the stream *is*
    /// the correlation, and a slow one cannot stall a fast one.
    pub async fn dispatch(
        &self,
        school: &Slug,
        capability: &str,
        payload: Value,
    ) -> Result<Value, AiError> {
        self.dispatch_with_timeout(school, capability, payload, self.inner.request_timeout)
            .await
    }

    /// [`AiBridge::dispatch`] with a per-call deadline, for capabilities whose
    /// cost is nothing like the default (a quick classification vs. a long
    /// generation).
    pub async fn dispatch_with_timeout(
        &self,
        school: &Slug,
        capability: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, AiError> {
        // The lease is taken before the timeout starts and released when this
        // function returns by any path, so an abandoned request frees the
        // worker's slot rather than leaking it.
        let lease = self.inner.registry.pick(capability)?;
        let id = Ulid::new().to_string();
        let deadline_ms = timeout.as_millis().min(u64::MAX as u128) as u64;
        let request = Request {
            id: id.clone(),
            school: school.as_str().to_string(),
            capability: capability.to_string(),
            deadline_ms,
            payload,
        };

        // The span names the capability, the worker, the school slug and this
        // call's own id — never the payload, which is the user's own text.
        let span = tracing::info_span!(
            "ai.request",
            "ai.capability" = capability,
            "ai.worker.id" = %lease.worker().id,
            school = school.as_str(),
            "ai.request.id" = %id,
            otel.status_code = tracing::field::Empty,
        );
        let metrics = self.inner.metrics();
        let attrs = [KeyValue::new("capability", capability.to_string())];
        let _in_flight = InFlight::started(metrics, &attrs);
        let started = Instant::now();

        let outcome = async {
            let call = async {
                let (mut send, mut recv) = lease.conn().open_bi().await.map_err(|e| {
                    AiError::Transport(format!("could not open a request stream: {e}"))
                })?;
                write_frame(&mut send, &request).await?;
                // FIN tells the service the request is complete — it can start
                // work without waiting to see whether more bytes follow.
                send.finish().map_err(|e| {
                    AiError::Transport(format!("could not finish the request: {e}"))
                })?;
                let response: Response = read_frame(&mut recv).await?;
                Ok::<_, AiError>(response)
            };

            let response = match tokio::time::timeout(timeout, call).await {
                Err(_) => {
                    metrics.ai_request_timeouts_total.add(1, &attrs);
                    tracing::warn!(
                        "ai.request.id" = %id,
                        capability,
                        "ai.worker.id" = %lease.worker().id,
                        deadline_ms,
                        "AI request timed out"
                    );
                    return Err(AiError::Timeout(deadline_ms));
                }
                Ok(result) => result?,
            };

            match response {
                Response::Ok {
                    id: got, payload, ..
                } if got == id => Ok(payload),
                Response::Err {
                    id: got,
                    code,
                    message,
                    ..
                } if got == id => Err(AiError::Remote { code, message }),
                // A mismatched id means the service is not tracking which
                // stream it is on. Nothing here depends on the id for
                // correlation, but the answer's *content* now belongs to some
                // other request, so it must not be handed back as this one's
                // result.
                Response::Ok { id: got, .. } | Response::Err { id: got, .. } => {
                    Err(AiError::IdMismatch { expected: id, got })
                }
            }
        }
        .instrument(span.clone())
        .await;

        metrics
            .ai_request_duration
            .record(started.elapsed().as_secs_f64(), &attrs);
        if outcome.is_err() {
            span.record("otel.status_code", "ERROR");
        }
        outcome
    }

    /// Hand the api-read path the router it dispatches into.
    ///
    /// Called once by [`crate::build_router`] with the *pre-layer* service, so
    /// a synthetic QUIC request skips the per-IP limiter, CORS and `ETag` (see
    /// the comment there). A second call keeps the first router: the process
    /// only ever builds one.
    pub(crate) fn arm_api(
        &self,
        router: axum::Router,
        tenants: Tenants,
        db_up: DbHealth,
        files_path: std::path::PathBuf,
        metrics: Metrics,
    ) {
        // Not warned about on a second call: the instruments are the same
        // process-global ones either way, so re-arming changes nothing.
        let _ = self.inner.metrics.set(metrics);
        if self
            .inner
            .api
            .set(ApiHandle {
                router,
                tenants,
                db_up,
                files_path,
            })
            .is_err()
        {
            tracing::warn!("the AI bridge api path was already armed — keeping the first router");
        }
    }

    /// Stop listening and close every service connection.
    pub fn close(&self) {
        self.inner.endpoint.close(0u32.into(), b"shutting down");
    }
}

/// Accept connections until the endpoint closes. One task per service.
async fn accept_loop(inner: Arc<Inner>) {
    while let Some(incoming) = inner.endpoint.accept().await {
        let inner = Arc::clone(&inner);
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => serve_connection(inner, conn).await,
                Err(e) => {
                    handshake_failed(inner.metrics(), "transport");
                    tracing::warn!(error = %e, "an AI service's QUIC handshake failed");
                }
            }
        });
    }
    tracing::info!("AI bridge listener stopped");
}

/// Run one service connection: handshake, register, wait for it to die,
/// deregister.
async fn serve_connection(inner: Arc<Inner>, conn: quinn::Connection) {
    let handshake = tokio::time::timeout(
        Duration::from_secs(AI_HANDSHAKE_TIMEOUT_SECS),
        register(&inner, &conn),
    )
    .await;

    let (worker_id, service, _control) = match handshake {
        Ok(Ok(registered)) => registered,
        Ok(Err(())) => return,
        Err(_) => {
            handshake_failed(inner.metrics(), "timeout");
            tracing::warn!("an AI service did not complete its handshake in time");
            conn.close(2u32.into(), b"handshake timeout");
            return;
        }
    };

    // The control stream is held open (`_control`) for the connection's life.
    // Nothing more is read on it: its closure, not a heartbeat frame, is how a
    // service says goodbye, and the QUIC idle timeout covers the case where it
    // dies without saying anything.
    //
    // Every *further* client-initiated stream is one api read or one blob
    // read. A service that opens none behaves exactly as it did before this
    // loop existed.
    let reason = loop {
        tokio::select! {
            reason = conn.closed() => break reason,
            accepted = conn.accept_bi() => match accepted {
                Ok((send, recv)) => {
                    tokio::spawn(serve_client_stream(Arc::clone(&inner), send, recv));
                }
                // The connection is going away; `closed()` has the real reason.
                Err(_) => break conn.closed().await,
            },
        }
    };
    inner.registry.remove(&worker_id);
    inner.record_worker_count();
    tracing::info!(
        "worker.id" = %worker_id,
        service = %service,
        reason = %reason,
        "AI service disconnected"
    );
}

/// Answer one client-initiated stream. Two shapes ride this path and are told
/// apart by the field each requires: an [`ApiRequest`] has `path`, a
/// [`BlobRequest`] has `file`. The api read is tried first, so every frame that
/// parsed as one before still does, byte for byte.
async fn serve_client_stream(
    inner: Arc<Inner>,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) {
    // Decoded as a `Value` first only so a frame that is JSON but not an
    // `ApiRequest` still gets its `id` echoed back — that id is what the
    // service correlates its own logs by.
    let raw: Value = match read_frame(&mut recv).await {
        Ok(raw) => raw,
        Err(e) => {
            answer(
                &mut send,
                refusal(String::new(), String::new(), "malformed", e.to_string()),
            )
            .await;
            return;
        }
    };
    let id = string_field(&raw, "id");
    // Echoed verbatim on a refusal, before it is known to be a slug at all:
    // it is how the service tells which of its in-flight reads was refused.
    let school = string_field(&raw, "school");
    // Routed on the shape's required field rather than by parsing one and
    // falling back to the other: that fallback cloned the whole `Value` on
    // every api read. `path` still wins when a frame carries both.
    if raw.get("path").is_some() {
        let response = match serde_json::from_value::<ApiRequest>(raw) {
            Ok(request) => match read_api(&inner, request).await {
                Ok(response) => response,
                Err((code, message)) => refusal(id, school, code, message),
            },
            Err(e) => refusal(id, school, "malformed", e.to_string()),
        };
        answer(&mut send, response).await;
        return;
    }
    if raw.get("file").is_some() {
        match serde_json::from_value::<BlobRequest>(raw) {
            Ok(request) => serve_blob(&inner, &mut send, request).await,
            Err(e) => answer(&mut send, refusal(id, school, "malformed", e.to_string())).await,
        }
        return;
    }
    // Neither shape. Reported as the api-read refusal it has always been —
    // `path` is the field a frame this far off most likely meant to carry.
    let api_err = serde_json::from_value::<ApiRequest>(raw)
        .err()
        .map_or_else(String::new, |e| e.to_string());
    answer(&mut send, refusal(id, school, "malformed", api_err)).await;
}

/// One string field of a raw frame, or the empty string. Used for the fields a
/// refusal echoes back before the frame has been proved to be anything.
fn string_field(raw: &Value, name: &str) -> String {
    raw.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Answer one blob read: the [`BlobResponse`] header frame, then — on `Ok` —
/// exactly `size` raw bytes copied straight off disk, then FIN.
///
/// The bytes are streamed rather than buffered: a course-note attachment can
/// be the school's whole `max_file_bytes`, and this path exists precisely
/// because such a thing does not fit a frame.
async fn serve_blob(inner: &Inner, send: &mut quinn::SendStream, request: BlobRequest) {
    let id = request.id.clone();
    let school = request.school.clone();
    match open_blob(inner, request).await {
        Ok((header, mut file)) => {
            if let Err(e) = write_frame(send, &header).await {
                tracing::warn!("could not answer an AI service's blob read {id}: {e}");
            } else if let Err(e) = write_blob_body(&mut file, send).await {
                // The header already promised `size` bytes, so a short body
                // would read as a silently truncated file. Reset instead: the
                // service sees a broken stream and can ask again.
                tracing::warn!("blob read {id} failed after its header: {e}");
                let _ = send.reset(1u32.into());
                return;
            }
        }
        Err((code, message)) => {
            let refused = BlobResponse::Err {
                id,
                school,
                code: code.to_string(),
                message,
            };
            if let Err(e) = write_frame(send, &refused).await {
                tracing::warn!("could not refuse an AI service's blob read: {e}");
            }
        }
    }
    let _ = send.finish();
}

/// Copy the blob to the stream, one 64 KiB chunk at a time, giving each write
/// [`AI_BLOB_WRITE_STALL_SECS`] to make progress.
///
/// The deadline is per write, not over the transfer: a service that reads
/// slowly keeps getting fresh time, while one that opens the stream and never
/// reads blocks on the QUIC stream window and is cut loose instead of parking
/// this task and the open file for the connection's life.
async fn write_blob_body(
    file: &mut tokio::fs::File,
    send: &mut quinn::SendStream,
) -> std::io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = tokio::io::AsyncReadExt::read(file, &mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        tokio::time::timeout(
            Duration::from_secs(AI_BLOB_WRITE_STALL_SECS),
            send.write_all(&buf[..n]),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no progress for {AI_BLOB_WRITE_STALL_SECS}s"),
            )
        })??;
    }
}

/// Resolve, authorize and open one blob. `Err` is the refusal that becomes a
/// [`BlobResponse::Err`] code.
///
/// Scope is course-note attachments and nothing else: the id is looked up in
/// `course_note_file` alone, so a personal note's file id reads as `not_found`
/// rather than reaching another table's row. Authorization is the very guard
/// `download_file` applies over HTTP — [`can_view_course`] on the note's own
/// course — so the bridge widens who may ask, never what may be read.
async fn open_blob(
    inner: &Inner,
    request: BlobRequest,
) -> Result<(BlobResponse, tokio::fs::File), (&'static str, String)> {
    let api = inner.api.get().ok_or((
        "unavailable",
        "the api is not serving yet — retry".to_string(),
    ))?;
    if !api.db_up.is_up() {
        return Err((
            "unavailable",
            "the database socket is down — retry".to_string(),
        ));
    }
    let tenant = api.school(&request.school).await?;
    let (slug, db) = (tenant.slug, tenant.db);
    // This stream bypasses the router, so it also bypasses the route_layer the
    // module gate is — the entitlement is checked here by hand instead.
    // `chatbot` too: a school that did not buy the `ai` package sends no data
    // to an AI service, and these bytes are the largest thing it would send.
    for module in [Module::CourseNotes, Module::Chatbot] {
        if !tenant.modules.contains(module) {
            return Err((
                "module_disabled",
                format!("the `{slug}` school has no `{module}` module"),
            ));
        }
    }
    let user = principal(&db, request.on_behalf_of.as_deref()).await?;

    let missing = || {
        (
            "not_found",
            format!("no course note file `{}`", request.file),
        )
    };
    let unavailable = |e: crate::error::AppError| ("unavailable", e.to_string());
    let file = crate::db::course_note_file::read(&db, &CourseNoteFileId::from_key(&request.file))
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let note = crate::db::course_note::read(&db, file.get_course_note())
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    let course = crate::service::course::read(&db, note.get_course())
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    if !can_view_course(&course, &user, &db)
        .await
        .map_err(unavailable)?
    {
        return Err((
            "forbidden",
            format!(
                "`{}` may not view the course this file belongs to",
                user.get_id().key()
            ),
        ));
    }

    // The row exists but its blob does not: server-side damage (a lost volume
    // path), exactly as `download_file` reads it — not the service's `404`.
    let path = blob_path(&api.files_dir(&slug), file.get_id().key());
    let handle = tokio::fs::File::open(&path).await.map_err(|e| {
        (
            "unavailable",
            format!(
                "missing blob for course note file {}: {e}",
                file.get_id().key()
            ),
        )
    })?;
    // The header's `size` is what will actually be copied, so it is taken from
    // the file on disk rather than the row's column: a service reading exactly
    // `size` bytes must never over- or under-read.
    let size = handle
        .metadata()
        .await
        .map_err(|e| ("unavailable", format!("could not stat the blob: {e}")))?
        .len();
    Ok((
        BlobResponse::Ok {
            id: request.id,
            school: request.school,
            name: file.get_name().as_str().to_string(),
            content_type: file.get_content_type().as_str().to_string(),
            size,
        },
        handle,
    ))
}

/// Who a client-initiated read runs as.
///
/// Loaded live out of the *school's own* database — never the control one and
/// never trusted from the frame: a service holding a stale id must not act as a
/// user who has since been deleted or demoted, nor as a same-named user of
/// another school. With nobody named the principal is the synthetic `ai` role.
async fn principal(
    db: &Database,
    on_behalf_of: Option<&str>,
) -> Result<User, (&'static str, String)> {
    let Some(who) = on_behalf_of else {
        return Ok(User::ai_principal());
    };
    // Both the bare key (`abc`, as a REST path spells it) and the record form
    // (`user:abc`) are accepted.
    let key = who.strip_prefix("user:").unwrap_or(who);
    crate::db::user::read(db, &UserId::from_key(key))
        .await
        .map_err(|e| ("unavailable", format!("could not load `{who}`: {e}")))?
        .ok_or_else(|| ("unknown_user", format!("no user `{who}`")))
}

/// The bridge-level refusal this request earns *before* anything is dispatched,
/// in the order the contract pins: the method first, then the allowlist. A
/// refused path is never handed to the router at all.
fn refuse_before_dispatch(method: Option<&str>, path: &str) -> Option<(&'static str, String)> {
    match method {
        None | Some("GET") => {}
        Some(other) => {
            return Some((
                "method_not_allowed",
                format!("the api bridge reads only — `{other}` is never dispatched"),
            ));
        }
    }
    if !crate::ai::api::path_allowed(path) {
        return Some((
            "path_not_allowed",
            format!("`{path}` is not a path AI services may read"),
        ));
    }
    None
}

/// Validate, dispatch into the router, and turn its answer into a frame.
///
/// `Err` is a bridge-level refusal (code, message). Anything the router itself
/// answered — `401`, `403`, `404` included — comes back as
/// [`ApiResponse::Ok`] carrying that status: the service asked and the API
/// replied, which is not a transport failure.
async fn read_api(
    inner: &Inner,
    request: ApiRequest,
) -> Result<ApiResponse, (&'static str, String)> {
    if let Some(refusal) = refuse_before_dispatch(request.method.as_deref(), &request.path) {
        return Err(refusal);
    }

    // Armed by `build_router`, which runs after the listener binds: a service
    // that dials in inside that boot window is told to retry, never panicked on.
    let api = inner.api.get().ok_or((
        "unavailable",
        "the api is not serving yet — retry".to_string(),
    ))?;
    dispatch_api(api, inner.metrics(), request).await
}

/// The half of [`read_api`] that needs only the armed handle: liveness,
/// principal, dispatch, and framing the router's answer. Split out from the
/// listener so the refusal codes it owns can be exercised without a QUIC
/// endpoint (see this module's tests).
async fn dispatch_api(
    api: &ApiHandle,
    metrics: &Metrics,
    request: ApiRequest,
) -> Result<ApiResponse, (&'static str, String)> {
    let ApiRequest {
        id,
        school,
        path,
        query,
        on_behalf_of,
        ..
    } = request;

    // The outer db guard is one of the layers this path deliberately skips, so
    // the same liveness check happens here instead: a query issued against a
    // dead socket hangs rather than failing (see [`DbHealth`]).
    if !api.db_up.is_up() {
        return Err((
            "unavailable",
            "the database socket is down — retry".to_string(),
        ));
    }

    // The school comes off the frame, so this is where a service naming a
    // stranger's slug (or a suspended school's) is stopped — before a row of
    // anyone's data is read.
    let tenant = api.school(&school).await?;
    let db = tenant.db.clone();
    // The role itself is re-read again by the extractor on the dispatched
    // request; see [`principal`] for why it is never taken from the frame.
    let user = principal(&db, on_behalf_of.as_deref()).await?;

    let target = match query.as_deref() {
        Some(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path.clone(),
    };
    let mut dispatched = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(&target)
        .body(axum::body::Body::empty())
        .map_err(|e| {
            (
                "malformed",
                format!("`{target}` is not a request target: {e}"),
            )
        })?;
    // An extension cannot be set from outside the process, which is what makes
    // this principal unforgeable over HTTP (see [`AiPrincipal`]).
    dispatched.extensions_mut().insert(AiPrincipal(user));
    // The dispatched request carries no cookie, so the school is handed over
    // in the extension the shadow `State` reads first.
    // Carries the school's entitlements too, so the router's module gate
    // refuses a disabled nest on this path exactly as it does over HTTP.
    dispatched.extensions_mut().insert(TenantExt {
        slug: tenant.slug,
        db,
        modules: tenant.modules,
    });

    // This dispatch skips every layer, so it also skips the request span and
    // the request metrics the HTTP edge records — they are recorded here
    // instead, off the route *template* the allowlist matched. The concrete
    // path holds record ids and never reaches a span or a label.
    let route = crate::ai::api::route_template(&path).unwrap_or("unmatched");
    let otel_name = format!("GET {route}");
    let span = tracing::info_span!(
        "http.request",
        otel.name = %otel_name,
        "http.request.method" = "GET",
        "http.route" = route,
        school = school.as_str(),
        "ai.origin" = true,
        "http.response.status_code" = tracing::field::Empty,
    );
    metrics.request_started("GET");
    let started = Instant::now();

    let response = tokio::time::timeout(
        Duration::from_secs(REQUEST_TIMEOUT_SECS),
        api.router.clone().oneshot(dispatched),
    )
    .instrument(span.clone())
    .await
    .map_err(|_| {
        metrics.request_finished("GET", route, 504, Some(&school), started.elapsed());
        (
            "unavailable",
            format!("`{target}` did not answer within {REQUEST_TIMEOUT_SECS}s"),
        )
    })?
    .expect("an axum router is infallible");

    let status = response.status().as_u16();
    // Recorded as soon as the router has answered: what follows is bridge
    // framing, not the HTTP request the caller made.
    metrics.request_finished("GET", route, status, Some(&school), started.elapsed());
    span.record("http.response.status_code", i64::from(status));
    let body = axum::body::to_bytes(response.into_body(), AI_MAX_FRAME_BYTES)
        .await
        .map_err(|e| {
            (
                "too_large",
                format!("the answer to `{target}` does not fit a frame: {e}"),
            )
        })?;
    let body = if body.is_empty() {
        // `204`s and empty error bodies are a real answer, not a parse failure.
        Value::Null
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            (
                "not_json",
                format!("`{target}` answered with a body that is not JSON: {e}"),
            )
        })?
    };

    Ok(ApiResponse::Ok {
        id,
        school,
        status,
        body,
    })
}

fn refusal(id: String, school: String, code: &str, message: String) -> ApiResponse {
    ApiResponse::Err {
        id,
        school,
        code: code.to_string(),
        message,
    }
}

/// Write one answer frame and finish the stream.
///
/// An `Ok` too big to frame is downgraded to a `too_large` refusal rather than
/// dropped: a service that got no frame at all would wait out its own deadline
/// to learn nothing.
async fn answer(send: &mut quinn::SendStream, response: ApiResponse) {
    if let Err(e) = write_answer(send, &response).await {
        tracing::warn!("could not answer an AI service's api read: {e}");
    }
    let _ = send.finish();
}

/// Frame one answer, downgrading an oversize `Ok` as above. [`write_frame`]
/// checks the size *before* it writes a byte, so the refusal never follows a
/// half-written frame. Generic over the writer so the downgrade can be asserted
/// against a buffer.
async fn write_answer<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    response: &ApiResponse,
) -> Result<(), FrameError> {
    let written = write_frame(w, response).await;
    if let (Err(FrameError::TooLarge(_)), ApiResponse::Ok { id, school, .. }) = (&written, response)
    {
        return write_frame(
            w,
            &refusal(
                id.clone(),
                school.clone(),
                "too_large",
                format!("the answer exceeds the {AI_MAX_FRAME_BYTES}-byte frame limit"),
            ),
        )
        .await;
    }
    written
}

/// Read the `Hello`, decide, answer, and register on success.
///
/// `Err(())` means the connection was refused and already closed — the caller
/// has nothing left to clean up.
type Control = (quinn::SendStream, quinn::RecvStream);
async fn register(
    inner: &Inner,
    conn: &quinn::Connection,
) -> Result<(String, String, Control), ()> {
    let (mut send, mut recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            handshake_failed(inner.metrics(), "no_control_stream");
            tracing::warn!(error = %e, "an AI service opened no control stream");
            return Err(());
        }
    };

    let hello: Hello = match read_frame(&mut recv).await {
        Ok(hello) => hello,
        Err(e) => {
            handshake_failed(inner.metrics(), "malformed_hello");
            tracing::warn!(error = %e, "an AI service sent an unreadable Hello");
            refuse(&mut send, conn, RejectCode::Malformed, &e.to_string()).await;
            return Err(());
        }
    };

    if !protocol_matches(&hello) {
        let message = format!(
            "this backend speaks {AI_PROTOCOL}, the service announced {}",
            hello.protocol
        );
        handshake_failed(inner.metrics(), "bad_version");
        tracing::warn!(service = %hello.service, reason = "bad_version", "AI service rejected: {message}");
        refuse(&mut send, conn, RejectCode::UnsupportedProtocol, &message).await;
        return Err(());
    }
    if !constant_time_eq(hello.token.as_bytes(), inner.token.as_bytes()) {
        handshake_failed(inner.metrics(), "bad_token");
        tracing::warn!(
            service = %hello.service,
            reason = "bad_token",
            "AI service rejected: bad shared token"
        );
        refuse(&mut send, conn, RejectCode::Unauthorized, "invalid token").await;
        return Err(());
    }
    if hello.capabilities.is_empty() {
        handshake_failed(inner.metrics(), "bad_capabilities");
        tracing::warn!(
            service = %hello.service,
            reason = "bad_capabilities",
            "AI service rejected: declared no capabilities"
        );
        refuse(
            &mut send,
            conn,
            RejectCode::NoCapabilities,
            "declare at least one capability",
        )
        .await;
        return Err(());
    }

    let worker_id = Ulid::new().to_string();
    let max_concurrent = clamp_concurrency(hello.max_concurrent);
    let greeting = Greeting::Welcome {
        worker_id: worker_id.clone(),
        protocol: AI_PROTOCOL.to_string(),
    };
    if let Err(e) = write_frame(&mut send, &greeting).await {
        handshake_failed(inner.metrics(), "welcome_failed");
        tracing::warn!(service = %hello.service, error = %e, "could not welcome an AI service");
        return Err(());
    }

    // Registered only after the welcome is on the wire: a service that never
    // learns it was accepted must not start receiving requests.
    inner.registry.insert(
        worker_id.clone(),
        hello.service.clone(),
        hello.capabilities.clone(),
        max_concurrent,
        conn.clone(),
    );
    inner.record_worker_count();
    tracing::info!(
        "worker.id" = %worker_id,
        service = %hello.service,
        capabilities = hello.capabilities.len(),
        max_concurrent,
        "AI service registered"
    );
    Ok((worker_id, hello.service, (send, recv)))
}

/// Send a rejection the service can act on, then close. Best-effort: a peer
/// that has already vanished simply gets closed.
async fn refuse(
    send: &mut quinn::SendStream,
    conn: &quinn::Connection,
    code: RejectCode,
    message: &str,
) {
    let greeting = Greeting::Rejected {
        code,
        message: message.to_string(),
    };
    let _ = write_frame(send, &greeting).await;
    let _ = send.finish();
    // Give the frame a moment to leave before the connection close overtakes
    // it, so the service reports the real reason instead of a bare reset.
    let _ = tokio::time::timeout(Duration::from_millis(200), send.stopped()).await;
    conn.close(1u32.into(), message.as_bytes());
}

/// Compare two secrets without leaking their common prefix length through
/// timing. Length difference is not secret (and cannot be hidden here), but
/// the byte comparison must not short-circuit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One dispatched-and-unanswered AI request, counted for as long as it lives.
///
/// `AiBridge::send`'s future is dropped whenever its caller goes away (an HTTP
/// client disconnecting, an outer timeout), and a decrement written after the
/// `.await` is skipped on exactly those paths — leaking the gauge upwards for
/// the process's whole life. `Drop` covers every exit.
struct InFlight<'a> {
    metrics: &'a Metrics,
    attrs: &'a [KeyValue],
}

impl<'a> InFlight<'a> {
    fn started(metrics: &'a Metrics, attrs: &'a [KeyValue]) -> Self {
        metrics.ai_requests_inflight.add(1, attrs);
        Self { metrics, attrs }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.metrics.ai_requests_inflight.add(-1, self.attrs);
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::get;

    use super::{
        AI_MAX_FRAME_BYTES, ApiHandle, ApiRequest, ApiResponse, Metrics, constant_time_eq,
        refuse_before_dispatch,
    };
    use crate::ai::protocol::read_frame;
    use crate::tenant::DEMO_SLUG;

    /// An armed handle serving `router`. The database is never touched by these
    /// reads (nobody is named, so the principal is synthetic), but the handle
    /// carries one exactly as the live path does.
    async fn armed(router: axum::Router) -> ApiHandle {
        ApiHandle {
            router,
            tenants: crate::database::init_mem_tenants()
                .await
                .expect("in-memory deployment"),
            db_up: Default::default(),
            files_path: std::env::temp_dir(),
        }
    }

    /// A read of `/notes` as the service itself.
    fn read_of(path: &str) -> ApiRequest {
        ApiRequest {
            id: "trace-1".to_string(),
            school: DEMO_SLUG.to_string(),
            path: path.to_string(),
            query: None,
            on_behalf_of: None,
            method: None,
        }
    }

    /// The refusal code, or a panic naming the answer that was not one.
    fn refused(answer: Result<ApiResponse, (&'static str, String)>) -> &'static str {
        match answer {
            Err((code, _)) => code,
            Ok(other) => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_refused_as_not_json() {
        // No allowlisted route answers text today, so this stands in for the
        // day one does: the service is told the body was unreadable rather
        // than handed a frame whose `body` field silently became a string.
        let router = axum::Router::new().route("/notes", get(|| async { "plain text" }));
        let api = armed(router).await;
        assert_eq!(
            refused(super::dispatch_api(&api, &Metrics::noop(), read_of("/notes")).await),
            "not_json"
        );
    }

    #[tokio::test]
    async fn an_empty_body_is_a_null_answer_not_a_parse_failure() {
        // The other side of the same branch: a `204` has nothing to parse and
        // must not read as `not_json`.
        let router = axum::Router::new().route(
            "/notes",
            get(|| async { axum::http::StatusCode::NO_CONTENT }),
        );
        let api = armed(router).await;
        match super::dispatch_api(&api, &Metrics::noop(), read_of("/notes")).await {
            Ok(ApiResponse::Ok { status, body, .. }) => {
                assert_eq!(status, 204);
                assert_eq!(body, serde_json::Value::Null);
            }
            other => panic!("an empty body is an Ok answer: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_router_answer_over_the_frame_cap_is_refused_as_too_large() {
        // The read-side cap: the body is drained with a limit, so an answer
        // that cannot fit a frame is refused instead of being buffered whole.
        let router = axum::Router::new().route(
            "/notes",
            get(|| async { "x".repeat(AI_MAX_FRAME_BYTES + 1) }),
        );
        let api = armed(router).await;
        assert_eq!(
            refused(super::dispatch_api(&api, &Metrics::noop(), read_of("/notes")).await),
            "too_large"
        );
    }

    #[tokio::test]
    async fn an_oversize_ok_is_downgraded_to_a_too_large_refusal() {
        // The write-side cap: a body that passed the read limit can still
        // overflow once it is wrapped in its frame. The service must get a
        // refusal it can log, never silence it waits out — and, because the
        // size is checked before any byte is written, nothing of the oversize
        // frame may precede it on the stream.
        let big = ApiResponse::Ok {
            id: "trace-1".to_string(),
            school: DEMO_SLUG.to_string(),
            status: 200,
            body: serde_json::Value::String("x".repeat(AI_MAX_FRAME_BYTES)),
        };
        let mut wire = Vec::new();
        super::write_answer(&mut wire, &big)
            .await
            .expect("the downgrade is written");

        let mut rest = &wire[..];
        match read_frame::<_, ApiResponse>(&mut rest).await {
            Ok(ApiResponse::Err { id, code, .. }) => {
                assert_eq!(code, "too_large");
                assert_eq!(id, "trace-1", "the trace id survives the downgrade");
            }
            other => panic!("expected a too_large refusal on the wire: {other:?}"),
        }
        assert!(
            rest.is_empty(),
            "{} bytes of the oversize frame were written before the refusal",
            rest.len()
        );
    }

    #[test]
    fn only_a_get_on_an_allowlisted_path_is_ever_dispatched() {
        // An absent method means GET, which is the whole api the bridge offers.
        assert!(refuse_before_dispatch(None, "/notes").is_none());
        assert!(refuse_before_dispatch(Some("GET"), "/notes").is_none());

        let code = |method, path| refuse_before_dispatch(method, path).map(|(code, _)| code);
        assert_eq!(code(Some("DELETE"), "/notes"), Some("method_not_allowed"));
        assert_eq!(code(Some("get"), "/notes"), Some("method_not_allowed"));
        assert_eq!(code(None, "/users"), Some("path_not_allowed"));
        // Order is pinned: the method is judged before the path, so a write
        // attempt reads as a write attempt whatever it was aimed at.
        assert_eq!(code(Some("POST"), "/nope"), Some("method_not_allowed"));
    }

    #[tokio::test]
    async fn token_comparison_matches_only_exact_secrets() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(constant_time_eq(b"", b""));
    }
}
