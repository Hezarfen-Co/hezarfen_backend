//! The QUIC listener AI services dial into, and the request path out to them.
//!
//! See [`super::protocol`] for the wire shape. The backend listens rather than
//! dials so a service can sit behind NAT, restart without the backend needing
//! to know its address, and scale out by opening a second connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use ulid::Ulid;

use crate::ai::error::AiError;
use crate::ai::protocol::{
    Greeting, Hello, RejectCode, Request, Response, protocol_matches, read_frame, write_frame,
};
use crate::ai::registry::{AiRegistry, WorkerSnapshot, clamp_concurrency};
use crate::ai::tls;
use crate::constant::{AI_HANDSHAKE_TIMEOUT_SECS, AI_PROTOCOL};

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

struct Inner {
    registry: AiRegistry,
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
    let reason = conn.closed().await;
    inner.registry.remove(&worker_id);
    tracing::info!("AI service `{service}` ({worker_id}) at {remote} disconnected: {reason}");
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
    use super::constant_time_eq;

    #[tokio::test]
    async fn token_comparison_matches_only_exact_secrets() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secrets"));
        assert!(constant_time_eq(b"", b""));
    }
}
