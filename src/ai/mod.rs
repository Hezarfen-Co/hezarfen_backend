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
//! * [`protocol`] — frames on the wire
//! * [`server`] — the listener, handshake, and [`server::AiBridge::dispatch`]
//! * [`registry`] — who is connected and who gets the next request
//! * [`tls`] — the listener's certificate
//!
//! Nothing here is wired into a route yet: this is the transport, and the
//! individual AI features land on top of it.

pub mod error;
pub mod protocol;
pub mod registry;
pub mod server;
pub mod tls;

pub use error::AiError;
pub use registry::{AiRegistry, WorkerSnapshot};
pub use server::{AiBridge, BridgeConfig};
