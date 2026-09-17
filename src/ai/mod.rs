//! Talking to the AI services.
//!
//! The AI features live in separate processes (separate repos, likely separate
//! languages), so this is the only seam between them and the API. It is a QUIC
//! bridge: the backend listens, services dial in and register the capabilities
//! they serve, and each request rides its own QUIC bidirectional stream over
//! the service's single long-lived connection.
//!
//! Why QUIC rather than HTTP: stream multiplexing is in the transport, so N
//! concurrent requests to one service need no correlation-id bookkeeping and
//! no head-of-line blocking — a 30-second inference on stream 7 does not delay
//! the answer on stream 9. Connection setup happens once, not per request.
//!
//! * [`api`] — which REST paths a service may read back through the bridge
//! * [`protocol`] — frames on the wire
//! * [`capability`] — the capabilities the **backend** serves, and their scopes
//! * [`chat`] — the JSON payloads carried for the `chat.reply` capability
//! * [`rag`] — the `rag.index` payloads, and the course-note dispatch behind them
//! * [`rag_chat`] — the `rag.chat` payloads: a scoped question and the citations behind its answer
//! * [`podcast`] — the `podcast.*` job payloads, the relay behind submit/cancel,
//!   and the job-state report the service calls back
//! * [`server`] — the listener, handshake, and [`server::AiBridge::dispatch`]
//! * [`registry`] — who is connected here, and who gets the next request
//! * [`tls`] — the listener's certificate
//!
//! The transport is generic; the features land on top of it. So far that is
//! the chatbot (`web::chatbot`), which routes on the `chat.reply` capability,
//! course-note indexing (`web::course_notes`), which routes on `rag.index`,
//! and the podcast relay (`web::podcast`), which dispatches `podcast.submit`/
//! `podcast.cancel`, serves `podcast.report`, and streams the ingested audio.

pub mod api;
pub mod capability;
pub mod chat;
pub mod error;
pub mod insight;
pub mod podcast;
pub mod protocol;
pub mod rag;
pub mod rag_chat;
pub mod registry;
pub mod server;
pub mod tls;

pub use chat::{ChatReplyPayload, ChatRequestPayload, ChatRole, ChatTurn};
pub use error::AiError;
pub use insight::{
    ClassRequest, ClassResponse, RefreshRequest, RefreshResponse, StudentRequest, StudentResponse,
};
pub use rag::{RagFile, RagIndexPayload};
pub use rag_chat::{
    RagChatReplyPayload, RagChatRequestPayload, RagCitation, RagScopePair, RagTurn,
};
pub use registry::{AiRegistry, WorkerSnapshot};
pub use server::{AiBridge, BridgeConfig};

use crate::config::Config;

/// Start the QUIC bridge the AI services dial into, if one is configured.
///
/// Unconfigured is a supported deployment, not a degraded one: the school API
/// predates the AI features and must keep running without them, so an absent
/// `AI_QUIC_ADDR` returns `None` and nothing else changes. A *misconfigured*
/// bridge is the opposite — a bad address, an unreadable certificate or a
/// missing token means the operator asked for AI and would otherwise get a
/// silently dead feature, so that fails the boot.
///
/// In particular a set address with no `AI_SHARED_TOKEN` must never fall back
/// to listening: an unauthenticated bridge would let anyone who can reach the
/// UDP port register a worker and answer real users' messages.
pub async fn start_bridge(cfg: &Config) -> Result<Option<AiBridge>, AiError> {
    let Some(raw_addr) = cfg.ai_quic_addr.as_deref() else {
        tracing::info!("AI bridge disabled (AI_QUIC_ADDR unset)");
        return Ok(None);
    };
    let addr = raw_addr
        .parse()
        .map_err(|e| AiError::Setup(format!("AI_QUIC_ADDR `{raw_addr}` is not an address: {e}")))?;
    let token = cfg
        .ai_shared_token
        .clone()
        .ok_or_else(|| AiError::Setup("AI_SHARED_TOKEN must be set when AI_QUIC_ADDR is".into()))?;
    let bridge = AiBridge::bind(BridgeConfig {
        addr,
        token,
        cert_path: cfg.ai_tls_cert.clone(),
        key_path: cfg.ai_tls_key.clone(),
        request_timeout: std::time::Duration::from_secs(cfg.ai_request_timeout_secs),
    })
    .await?;
    Ok(Some(bridge))
}
