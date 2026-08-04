//! Machinery shared by every WebSocket room.
//!
//! Two rooms speak WebSocket in this crate — the student exam room
//! ([`super::exam_ws`]) and the collaborative whiteboard ([`super::board_ws`])
//! — and they share less than they look like they do. The exam room only ever
//! replies to the socket that spoke; the board room fans out to everyone else
//! on the board. What they genuinely share is the plumbing *around* that
//! difference: how a frame goes out, how an incoming message is classified, how
//! an [`AppError`] becomes words a client may read, and how a client's
//! correlation id is echoed back untouched.
//!
//! Deliberately **not** here, and the reason each time:
//!
//! - the `select!` loop — the board's third arm is a fan-out receiver the exam
//!   room has no analogue of, and the two loops' terminal conditions are
//!   different domain facts. A trait to unify them would be longer than both
//!   loops together and would still need a null receiver for the exam room.
//! - the pre-upgrade gates — the *pattern* (every gate before `on_upgrade`, so
//!   a refusal is an HTTP status and not an instant close) is shared, but the
//!   shared code is the single `ws.on_upgrade(…)` line at the end of it. The
//!   gates themselves are each room's own domain rules.
//! - presence — the exam room counts sockets per attempt to pair the last one
//!   out with a `left_at` stamp ([`crate::state::ExamPresence`]); the board
//!   room's join/leave is the fan-out subscription itself
//!   ([`crate::state::BoardHub`]), which counts its own sockets and has no
//!   paired database write to serialize against.

use axum::extract::ws::{Message, Utf8Bytes, WebSocket};
use serde_json::{Value, json};

use crate::error::AppError;

/// Errors that end a room: the peer went away, or the room reached a terminal
/// state and its closing frame was sent.
pub(crate) struct RoomClosed;

/// Send one JSON frame. A failed send means the peer is gone, which ends the
/// room — every caller propagates it with `?`.
pub(crate) async fn send(socket: &mut WebSocket, frame: Value) -> Result<(), RoomClosed> {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|_| RoomClosed)
}

/// The best-effort closing handshake a room ends with — a bare TCP teardown
/// reads as an error on the client; a Close frame reads as "the room is over".
pub(crate) async fn close(socket: &mut WebSocket) {
    let _ = socket.send(Message::Close(None)).await;
}

/// What `socket.recv()` handed us, in the three shapes a room cares about.
pub(crate) enum Incoming {
    /// A text frame — the only kind either protocol speaks.
    Text(Utf8Bytes),
    /// A frame carrying nothing for this protocol: ping/pong are answered by
    /// axum itself, and neither room speaks binary. Read the next one.
    Ignore,
    /// Close, transport error, or end of stream: the room is over.
    Gone,
}

pub(crate) fn classify(incoming: Option<Result<Message, axum::Error>>) -> Incoming {
    match incoming {
        Some(Ok(Message::Text(text))) => Incoming::Text(text),
        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => Incoming::Gone,
        Some(Ok(_)) => Incoming::Ignore,
    }
}

/// Attach the request's correlation id, if it sent one. Omitted stays omitted
/// — never `null` — so a client that sends no `client_seq` sees byte-identical
/// frames.
pub(crate) fn with_client_seq(frame: &mut Value, client_seq: Option<u64>) {
    if let Some(client_seq) = client_seq {
        frame["client_seq"] = json!(client_seq);
    }
}

/// The public words for an error — the same strings the HTTP layer would use,
/// with internals logged under `room` (`"exam room"`, `"board room"`) and never
/// sent.
pub(crate) fn public_message(err: &AppError, room: &str) -> String {
    match err {
        AppError::Validation(err) => err.to_string(),
        AppError::NotFound => "not found".to_string(),
        AppError::Unauthorized => "unauthorized".to_string(),
        AppError::Forbidden(message) => (*message).to_string(),
        AppError::Conflict(message) => (*message).to_string(),
        AppError::ConflictOwned(message) | AppError::ConflictCoded { message, .. } => {
            message.clone()
        }
        AppError::PayloadTooLarge(message) => message.clone(),
        AppError::TooManyRequests { .. } => "too many requests".to_string(),
        AppError::DbUnavailable => {
            tracing::warn!("{room}: database reconnecting");
            "database reconnecting — retry shortly".to_string()
        }
        AppError::DbTimeout => {
            tracing::error!("{room}: database timed out");
            "the database timed out — reload the room".to_string()
        }
        AppError::Db(_) | AppError::Internal(_) => {
            tracing::error!("{room} error: {err}");
            "internal server error".to_string()
        }
    }
}
