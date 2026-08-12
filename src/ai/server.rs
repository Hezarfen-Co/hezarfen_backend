//! The QUIC listener AI services dial into, and the request path out to them.
//!
//! See [`super::protocol`] for the wire shape. The backend listens rather than
//! dials so a service can sit behind NAT, restart without the backend needing
//! to know its address, and scale out by opening a second connection.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tower::ServiceExt;
use ulid::Ulid;

use crate::ai::error::AiError;
use crate::ai::protocol::{
    ApiRequest, ApiResponse, FrameError, Greeting, Hello, RejectCode, Request, Response,
    protocol_matches, read_frame, write_frame,
};
use crate::ai::registry::{AiRegistry, WorkerSnapshot, clamp_concurrency};
use crate::ai::tls;
use crate::constant::{
    AI_HANDSHAKE_TIMEOUT_SECS, AI_MAX_FRAME_BYTES, AI_PROTOCOL, REQUEST_TIMEOUT_SECS,
};
use crate::database::Database;
use crate::domain::user::{User, UserId};
use crate::state::DbHealth;
use crate::web::extractor::AiPrincipal;

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
/// database the acting principal is loaded from, and the liveness flag that
/// stands in for the HTTP db guard this path bypasses.
struct ApiHandle {
    router: axum::Router,
    db: Database,
    db_up: DbHealth,
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
    pub async fn dispatch(&self, capability: &str, payload: Value) -> Result<Value, AiError> {
        self.dispatch_with_timeout(capability, payload, self.inner.request_timeout)
            .await
    }

    /// [`AiBridge::dispatch`] with a per-call deadline, for capabilities whose
    /// cost is nothing like the default (a quick classification vs. a long
    /// generation).
    pub async fn dispatch_with_timeout(
        &self,
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
            capability: capability.to_string(),
            deadline_ms,
            payload,
        };

        let call = async {
            let (mut send, mut recv) =
                lease.conn().open_bi().await.map_err(|e| {
                    AiError::Transport(format!("could not open a request stream: {e}"))
                })?;
            write_frame(&mut send, &request).await?;
            // FIN tells the service the request is complete — it can start work
            // without waiting to see whether more bytes follow.
            send.finish()
                .map_err(|e| AiError::Transport(format!("could not finish the request: {e}")))?;
            let response: Response = read_frame(&mut recv).await?;
            Ok::<_, AiError>(response)
        };

        let response = match tokio::time::timeout(timeout, call).await {
            Err(_) => {
                tracing::warn!(
                    "AI request {id} for `{capability}` timed out after {deadline_ms}ms on worker {}",
                    lease.worker().id
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
            } if got == id => Err(AiError::Remote { code, message }),
            // A mismatched id means the service is not tracking which stream it
            // is on. Nothing here depends on the id for correlation, but the
            // answer's *content* now belongs to some other request, so it must
            // not be handed back as this one's result.
            Response::Ok { id: got, .. } | Response::Err { id: got, .. } => {
                Err(AiError::IdMismatch { expected: id, got })
            }
        }
    }

    /// Hand the api-read path the router it dispatches into.
    ///
    /// Called once by [`crate::build_router`] with the *pre-layer* service, so
    /// a synthetic QUIC request skips the per-IP limiter, CORS and `ETag` (see
    /// the comment there). A second call keeps the first router: the process
    /// only ever builds one.
    pub(crate) fn arm_api(&self, router: axum::Router, db: Database, db_up: DbHealth) {
        if self.inner.api.set(ApiHandle { router, db, db_up }).is_err() {
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
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => serve_connection(inner, conn, remote).await,
                Err(e) => tracing::warn!("AI service handshake from {remote} failed: {e}"),
            }
        });
    }
    tracing::info!("AI bridge listener stopped");
}

/// Run one service connection: handshake, register, wait for it to die,
/// deregister.
async fn serve_connection(inner: Arc<Inner>, conn: quinn::Connection, remote: SocketAddr) {
    let handshake = tokio::time::timeout(
        Duration::from_secs(AI_HANDSHAKE_TIMEOUT_SECS),
        register(&inner, &conn, remote),
    )
    .await;

    let (worker_id, service, _control) = match handshake {
        Ok(Ok(registered)) => registered,
        Ok(Err(())) => return,
        Err(_) => {
            tracing::warn!("AI service at {remote} did not complete its handshake in time");
            conn.close(2u32.into(), b"handshake timeout");
            return;
        }
    };

    // The control stream is held open (`_control`) for the connection's life.
    // Nothing more is read on it: its closure, not a heartbeat frame, is how a
    // service says goodbye, and the QUIC idle timeout covers the case where it
    // dies without saying anything.
    //
    // Every *further* client-initiated stream is one api read. A service that
    // opens none behaves exactly as it did before this loop existed.
    let reason = loop {
        tokio::select! {
            reason = conn.closed() => break reason,
            accepted = conn.accept_bi() => match accepted {
                Ok((send, recv)) => {
                    tokio::spawn(serve_api_read(Arc::clone(&inner), send, recv));
                }
                // The connection is going away; `closed()` has the real reason.
                Err(_) => break conn.closed().await,
            },
        }
    };
    inner.registry.remove(&worker_id);
    tracing::info!("AI service `{service}` ({worker_id}) at {remote} disconnected: {reason}");
}

/// Answer one api read on its own stream: one [`ApiRequest`] in, one
/// [`ApiResponse`] out, then the stream is finished.
async fn serve_api_read(
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
                refusal(String::new(), "malformed", e.to_string()),
            )
            .await;
            return;
        }
    };
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let request: ApiRequest = match serde_json::from_value(raw) {
        Ok(request) => request,
        Err(e) => {
            answer(&mut send, refusal(id, "malformed", e.to_string())).await;
            return;
        }
    };

    let response = match read_api(&inner, request).await {
        Ok(response) => response,
        Err((code, message)) => refusal(id, code, message),
    };
    answer(&mut send, response).await;
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
    dispatch_api(api, request).await
}

/// The half of [`read_api`] that needs only the armed handle: liveness,
/// principal, dispatch, and framing the router's answer. Split out from the
/// listener so the refusal codes it owns can be exercised without a QUIC
/// endpoint (see this module's tests).
async fn dispatch_api(
    api: &ApiHandle,
    request: ApiRequest,
) -> Result<ApiResponse, (&'static str, String)> {
    let ApiRequest {
        id,
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

    // Loaded live, never trusted from the frame: a service holding a stale id
    // must not act as a user who has since been deleted or demoted. The role
    // itself is re-read again by the extractor on the dispatched request.
    let user = match &on_behalf_of {
        Some(who) => {
            // Both the bare key (`abc`, as a REST path spells it) and the
            // record form (`user:abc`) are accepted.
            let key = who.strip_prefix("user:").unwrap_or(who);
            User::read(&UserId::from_key(key), &api.db)
                .await
                .map_err(|e| ("unavailable", format!("could not load `{who}`: {e}")))?
                .ok_or_else(|| ("unknown_user", format!("no user `{who}`")))?
        }
        None => User::ai_principal(),
    };

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

    let response = tokio::time::timeout(
        Duration::from_secs(REQUEST_TIMEOUT_SECS),
        api.router.clone().oneshot(dispatched),
    )
    .await
    .map_err(|_| {
        (
            "unavailable",
            format!("`{target}` did not answer within {REQUEST_TIMEOUT_SECS}s"),
        )
    })?
    .expect("an axum router is infallible");

    let status = response.status().as_u16();
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

    Ok(ApiResponse::Ok { id, status, body })
}

fn refusal(id: String, code: &str, message: String) -> ApiResponse {
    ApiResponse::Err {
        id,
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
    if let (Err(FrameError::TooLarge(_)), ApiResponse::Ok { id, .. }) = (&written, response) {
        return write_frame(
            w,
            &refusal(
                id.clone(),
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
    remote: SocketAddr,
) -> Result<(String, String, Control), ()> {
    let (mut send, mut recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            tracing::warn!("AI service at {remote} opened no control stream: {e}");
            return Err(());
        }
    };

    let hello: Hello = match read_frame(&mut recv).await {
        Ok(hello) => hello,
        Err(e) => {
            tracing::warn!("AI service at {remote} sent an unreadable Hello: {e}");
            refuse(&mut send, conn, RejectCode::Malformed, &e.to_string()).await;
            return Err(());
        }
    };

    if !protocol_matches(&hello) {
        let message = format!(
            "this backend speaks {AI_PROTOCOL}, the service announced {}",
            hello.protocol
        );
        tracing::warn!(
            "AI service `{}` at {remote} rejected: {message}",
            hello.service
        );
        refuse(&mut send, conn, RejectCode::UnsupportedProtocol, &message).await;
        return Err(());
    }
    if !constant_time_eq(hello.token.as_bytes(), inner.token.as_bytes()) {
        tracing::warn!(
            "AI service `{}` at {remote} rejected: bad shared token",
            hello.service
        );
        refuse(&mut send, conn, RejectCode::Unauthorized, "invalid token").await;
        return Err(());
    }
    if hello.capabilities.is_empty() {
        tracing::warn!(
            "AI service `{}` at {remote} rejected: declared no capabilities",
            hello.service
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
        tracing::warn!(
            "could not welcome AI service `{}` at {remote}: {e}",
            hello.service
        );
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
    tracing::info!(
        "AI service `{}` ({worker_id}) at {remote} registered: {:?}, max_concurrent {max_concurrent}",
        hello.service,
        hello.capabilities
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

#[cfg(test)]
mod tests {
    use axum::routing::get;

    use super::{
        AI_MAX_FRAME_BYTES, ApiHandle, ApiRequest, ApiResponse, constant_time_eq,
        refuse_before_dispatch,
    };
    use crate::ai::protocol::read_frame;

    /// An armed handle serving `router`. The database is never touched by these
    /// reads (nobody is named, so the principal is synthetic), but the handle
    /// carries one exactly as the live path does.
    async fn armed(router: axum::Router) -> ApiHandle {
        ApiHandle {
            router,
            db: crate::database::init_mem().await.expect("in-memory db"),
            db_up: Default::default(),
        }
    }

    /// A read of `/notes` as the service itself.
    fn read_of(path: &str) -> ApiRequest {
        ApiRequest {
            id: "trace-1".to_string(),
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
            refused(super::dispatch_api(&api, read_of("/notes")).await),
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
        match super::dispatch_api(&api, read_of("/notes")).await {
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
            refused(super::dispatch_api(&api, read_of("/notes")).await),
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
