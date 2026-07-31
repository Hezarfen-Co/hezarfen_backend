//! The collaborative board room: a WebSocket at `GET /boards/{id}/ws`.
//!
//! This is where the drawing actually happens. REST next door
//! ([`super::boards`]) opens boards, reads the canvas and the history, and runs
//! the creator's commands; the room is the live half — one socket per
//! participant, every accepted stroke fanned out to the rest through
//! [`crate::state::BoardHub`].
//!
//! Two rules shape everything below.
//!
//! **Persist, then publish.** A stroke is fanned out only once
//! [`BoardStroke::append`] has returned `Ok`, so the channel can never carry a
//! mark the database refused (a locked board, a full canvas, a closed board).
//! The database is the canvas; the channel is a notification about it. A
//! client still dedupes by stroke `id` — a resync deliberately re-serves marks
//! it already drew — but it never sees the echo of its own strokes: those are
//! acknowledged with `saved` and filtered back out of its fan-out.
//!
//! **A clear deletes nothing.** `clear` bumps the board's epoch, so the live
//! canvas empties while every stroke stays stored. A join therefore replays the
//! *current epoch only* — history never rides this socket, it is an explicit
//! REST read (`GET /boards/{id}/history`). A client that reconnects with a
//! cursor from an older epoch is told `cleared` and given the whole current
//! epoch, never a diff from a cursor that no longer means anything.
//!
//! Wire protocol (JSON text frames):
//!
//! client → server
//! - `{"type":"join", after?, epoch?}` — replay the current epoch. `after` is
//!   the last stroke id already drawn and `epoch` the canvas it belongs to;
//!   send both to resume, neither for a full replay. A stale `epoch` gets a
//!   `cleared` frame and a full replay.
//! - `{"type":"stroke", payload, client_seq?}` — any participant
//! - `{"type":"clear"}` / `{"type":"lock", locked}` — **creator only**; anyone
//!   else gets `error{code:"forbidden"}` and *keeps the socket*
//! - `{"type":"ping"}`
//!
//! server → client
//! - `{"type":"state", board, epoch, locked, closed_at, creator, participants, now}`
//!   on connect and every [`BOARD_WS_TICK_SECS`]
//! - `{"type":"strokes", epoch, strokes:[{id, author, payload}]}` — one replay
//!   chunk ([`BOARD_REPLAY_CHUNK`] at a time), then
//!   `{"type":"synced", epoch, cursor}`
//! - `{"type":"stroke", id, author, payload, epoch}` — someone *else* drew
//! - `{"type":"saved", id, client_seq?}` — your stroke landed
//! - `{"type":"pong"}`
//! - `{"type":"error", code, message, client_seq?}`
//! - the five frames the REST routes publish, forwarded verbatim: `cleared`,
//!   `locked`, `closed`, `participants`, `deleted`
//!
//! `closed` and `deleted` end the room. `participants` is re-derived against
//! the database and drops this socket if it is no longer on the board — but the
//! notification is only the *prompt* half of that: every stroke, clear and lock
//! re-reads the board and re-checks the roster, so a socket that missed the
//! frame (it lagged, or the roster changed without one) is refused all the
//! same. A frame is never the authority here.

use std::collections::VecDeque;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::constant::{BOARD_REPLAY_CHUNK, BOARD_WS_TICK_SECS, MAX_BOARD_ID_LEN};
use crate::database::Database;
use crate::domain::board::{Board, BoardId};
use crate::domain::board_stroke::BoardStroke;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::state::AppState;
use crate::web::CurrentUser;
use crate::web::room::{self, Incoming, RoomClosed, send, with_client_seq};

/// How this room names itself in the logs [`room::public_message`] writes.
const ROOM: &str = "board room";

/// What the client asked for, tagged by `type`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientMessage {
    Join {
        /// The last stroke id this client already drew.
        #[serde(default)]
        after: Option<String>,
        /// The epoch that cursor belongs to. A mismatch means the canvas was
        /// cleared while the client was away, and the cursor is dead.
        #[serde(default)]
        epoch: Option<i64>,
    },
    Stroke {
        payload: String,
        /// The client's own correlation id, echoed verbatim on this message's
        /// `saved` or `error` and never read by the server.
        #[serde(default)]
        client_seq: Option<u64>,
    },
    Clear,
    Lock {
        locked: bool,
    },
    Ping,
}

/// Enter the caller's board room. Every gate runs *before* the upgrade so a
/// rejection is an HTTP status rather than an instant close: no session (401),
/// and a board that does not exist — or that the caller is not on, or that the
/// caller is a `parent` and so barred from the whiteboard outright — is the
/// same **404**, because a non-participant must not learn a board exists (the
/// rationale at `src/lib.rs`). Refusing the door is also what stops a parent
/// drawing: no socket, no `stroke` frame, and no per-stroke role read on the
/// hot path.
pub async fn board_ws(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    // Length-capped before the key is read back or echoed in a `state` frame.
    // A key this long names no board, so it answers like any other stranger's
    // board: 404.
    if id.len() > MAX_BOARD_ID_LEN || !user.get_role().at_least(Role::Student) {
        return Err(AppError::NotFound);
    }
    let board = Board::read(&BoardId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !board.is_participant(user.get_id()) {
        return Err(AppError::NotFound);
    }
    let board_id = board.get_id().clone();
    let user_id = user.get_id().clone();
    Ok(ws.on_upgrade(move |socket| room(socket, st, board_id, user_id)))
}

/// The room loop: the board's frames out, draw/clear/lock/ping in, until the
/// socket closes or the board ends (closed, deleted, or the caller taken off
/// it).
async fn room(mut socket: WebSocket, st: AppState, board: BoardId, user: UserId) {
    // Subscribe *before* anything is read out of the database: a stroke that
    // lands between a replay's read and this subscribe would otherwise be lost
    // outright, whereas one delivered twice is deduplicated by its id at the
    // client (which a resync forces anyway).
    let mut feed = st.board_hub.subscribe(board.key());
    // The ids this socket drew, in the order they were published. The hub has
    // no idea who is listening, so the room filters its own strokes back out
    // here — a client that had to ignore the echo of every mark it just drew
    // would be reimplementing this in every frontend.
    let mut mine = VecDeque::new();
    let mut tick = tokio::time::interval(Duration::from_secs(BOARD_WS_TICK_SECS));
    loop {
        let step = tokio::select! {
            // First tick fires immediately — the connect-time state frame.
            _ = tick.tick() => push_state(&mut socket, &board, &user, &st.db).await,
            frame = feed.recv() => match frame {
                Ok(frame) => forward(&mut socket, &frame, &board, &user, &mut mine, &st.db).await,
                // The room fell further behind than the hub's capacity, so
                // frames were dropped. Silently continuing here is the one bug
                // that corrupts a canvas permanently and reports nothing: say
                // so, then re-serve the whole current epoch from the database.
                Err(RecvError::Lagged(dropped)) => {
                    tracing::warn!("board room dropped {dropped} frames, resyncing");
                    // The dropped frames may have included this socket's own
                    // echoes, so the queue no longer lines up with the stream.
                    // The replay that follows carries those strokes anyway.
                    mine.clear();
                    resync(&mut socket, &board, &user, &st.db).await
                }
                Err(RecvError::Closed) => Err(RoomClosed),
            },
            incoming = socket.recv() => match room::classify(incoming) {
                Incoming::Text(text) => {
                    handle_message(&mut socket, text.as_str(), &board, &user, &mut mine, &st).await
                }
                Incoming::Gone => Err(RoomClosed),
                Incoming::Ignore => Ok(()),
            },
        };
        if step.is_err() {
            break;
        }
    }
    room::close(&mut socket).await;
    st.board_hub.leave(board.key());
}

type Step = Result<(), RoomClosed>;

/// The board as it stands right now, for a caller who must still be on it.
/// Every action re-reads through here: the roster, the lock and the epoch are
/// all live facts, and a frame this socket may never have seen is not an
/// authority over any of them.
async fn live_board(board: &BoardId, user: &UserId, db: &Database) -> Result<Board, AppError> {
    let board = Board::read(board, db).await?.ok_or(AppError::NotFound)?;
    if !board.is_participant(user) {
        return Err(AppError::Forbidden("you are no longer on this board"));
    }
    Ok(board)
}

/// Push a `state` frame, and end the room if the board is gone or the caller
/// has been taken off it.
async fn push_state(socket: &mut WebSocket, board: &BoardId, user: &UserId, db: &Database) -> Step {
    match live_board(board, user, db).await {
        Ok(board) => {
            send(
                socket,
                json!({
                    "type": "state",
                    "board": board.get_id().key(),
                    "epoch": board.get_epoch(),
                    "locked": board.is_locked(),
                    "closed_at": board.get_closed_at().map(|at| at.as_millis()),
                    "creator": board.get_creator().key(),
                    "participants": board
                        .get_participants()
                        .iter()
                        .map(|user| user.key())
                        .collect::<Vec<_>>(),
                    "now": Timestamp::now().as_millis(),
                }),
            )
            .await
        }
        Err(err) => {
            let frame = match err {
                AppError::NotFound => json!({ "type": "deleted" }),
                err => error_frame(&err, None),
            };
            let _ = send(socket, frame).await;
            Err(RoomClosed)
        }
    }
}

/// One frame off the hub, on its way to this socket. Forwarded verbatim — the
/// five REST shapes are the contract — with three of them acted on as well,
/// and this socket's own strokes dropped: it already got a `saved` for each.
async fn forward(
    socket: &mut WebSocket,
    frame: &str,
    board: &BoardId,
    user: &UserId,
    mine: &mut VecDeque<String>,
    db: &Database,
) -> Step {
    let parsed = serde_json::from_str::<Value>(frame).ok();
    // The channel is FIFO, so this socket's own strokes come back in the order
    // it published them: only the head can match, which keeps the check O(1)
    // and cannot swallow someone else's mark that happens to be next.
    //
    // ponytail: that ordering is an unenforced coupling — the head only ever
    // matches because `mine.push_back` immediately precedes `publish` in
    // `handle_message`, on a loop that is single-threaded per socket. Separate
    // those two lines (or publish from anywhere else) and this socket starts
    // receiving the echo of its own strokes; NO test catches it, because both
    // orders pass every existing assertion. Upgrade path: publish through one
    // helper that takes the queue and does both, so the pair cannot be split.
    if let Some(parsed) = &parsed
        && parsed["type"] == "stroke"
        && mine.front().is_some_and(|id| parsed["id"] == id.as_str())
    {
        mine.pop_front();
        return Ok(());
    }
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|_| RoomClosed)?;
    let kind = parsed.and_then(|frame| frame["type"].as_str().map(str::to_string));
    match kind.as_deref() {
        // The board ended. Nothing left to serve either way.
        Some("closed" | "deleted") => Err(RoomClosed),
        // Re-derived against the database rather than read off the frame: the
        // frame is the prompt, the row is the authority.
        Some("participants") => match live_board(board, user, db).await {
            Ok(_) => Ok(()),
            Err(err) => {
                let _ = send(socket, error_frame(&err, None)).await;
                Err(RoomClosed)
            }
        },
        _ => Ok(()),
    }
}

/// Frames were dropped: tell the client, then re-serve the current epoch in
/// full. A diff is impossible — the gap's contents are exactly what is unknown.
async fn resync(socket: &mut WebSocket, board: &BoardId, user: &UserId, db: &Database) -> Step {
    let live = match live_board(board, user, db).await {
        Ok(live) => live,
        Err(err) => {
            let frame = match err {
                AppError::NotFound => json!({ "type": "deleted" }),
                err => error_frame(&err, None),
            };
            let _ = send(socket, frame).await;
            return Err(RoomClosed);
        }
    };
    send(
        socket,
        json!({
            "type": "error",
            "code": "resync",
            "message": "the room fell behind — the canvas below replaces what you have",
        }),
    )
    .await?;
    replay(socket, board, live.get_epoch(), None, db).await
}

/// Serve one epoch, oldest first, [`BOARD_REPLAY_CHUNK`] rows at a time, and
/// close with the cursor the client can resume from. The loop runs until a
/// short chunk comes back — stopping at the first full one is how a replay
/// silently truncates at the chunk boundary.
async fn replay(
    socket: &mut WebSocket,
    board: &BoardId,
    epoch: i64,
    after: Option<String>,
    db: &Database,
) -> Step {
    let mut cursor = after;
    loop {
        let chunk = match BoardStroke::replay_current(board, epoch, cursor.as_deref(), db).await {
            Ok(chunk) => chunk,
            Err(err) => return send(socket, error_frame(&err, None)).await,
        };
        let short = chunk.len() < BOARD_REPLAY_CHUNK;
        if let Some(last) = chunk.last() {
            cursor = Some(last.get_id().key().to_string());
            send(
                socket,
                json!({
                    "type": "strokes",
                    "epoch": epoch,
                    "strokes": chunk.iter().map(stroke_body).collect::<Vec<_>>(),
                }),
            )
            .await?;
        }
        if short {
            break;
        }
    }
    send(
        socket,
        json!({ "type": "synced", "epoch": epoch, "cursor": cursor }),
    )
    .await
}

fn stroke_body(stroke: &BoardStroke) -> Value {
    json!({
        "id": stroke.get_id().key(),
        "author": stroke.get_author().key(),
        "payload": stroke.get_payload(),
    })
}

async fn handle_message(
    socket: &mut WebSocket,
    text: &str,
    board: &BoardId,
    user: &UserId,
    mine: &mut VecDeque<String>,
    st: &AppState,
) -> Step {
    let message = match serde_json::from_str::<ClientMessage>(text) {
        Ok(message) => message,
        Err(err) => {
            return send(
                socket,
                json!({
                    "type": "error",
                    "code": "invalid",
                    "message": format!("unrecognized message: {err}"),
                }),
            )
            .await;
        }
    };
    match message {
        ClientMessage::Ping => send(socket, json!({ "type": "pong" })).await,
        ClientMessage::Join { after, epoch } => {
            let live = match live_board(board, user, &st.db).await {
                Ok(live) => live,
                Err(err) => {
                    let _ = send(socket, error_frame(&err, None)).await;
                    return Err(RoomClosed);
                }
            };
            // A cursor is only meaningful inside the epoch it was taken from.
            // Against any other one the canvas was wiped in between: say so,
            // and replay the whole live canvas rather than a diff from a dead
            // cursor — which would leave the client staring at a blank board.
            let resume = match epoch {
                Some(epoch) if epoch != live.get_epoch() => {
                    send(
                        socket,
                        json!({
                            "type": "cleared",
                            "epoch": live.get_epoch(),
                            "by": Value::Null,
                        }),
                    )
                    .await?;
                    None
                }
                // Bounded like the board key it sits beside — both are record
                // keys of the same shape, and this one is echoed back as the
                // `synced` cursor.
                _ => after.filter(|after| after.len() <= MAX_BOARD_ID_LEN),
            };
            replay(socket, board, live.get_epoch(), resume, &st.db).await
        }
        ClientMessage::Stroke {
            payload,
            client_seq,
        } => {
            // Re-read for the roster *and* the epoch: the canvas may have been
            // cleared since this socket last heard anything.
            let saved = match live_board(board, user, &st.db).await {
                Ok(live) => {
                    BoardStroke::append(board, user, &payload, live.get_epoch(), &st.db).await
                }
                Err(err) => Err(err),
            };
            match saved {
                Ok(stroke) => {
                    // Persist, then publish: the fan-out only ever carries
                    // marks the database already accepted.
                    let mut fanned = stroke_body(&stroke);
                    fanned["type"] = json!("stroke");
                    fanned["epoch"] = json!(stroke.get_epoch());
                    mine.push_back(stroke.get_id().key().to_string());
                    st.board_hub.publish(board.key(), fanned.to_string());
                    let mut frame = json!({ "type": "saved", "id": stroke.get_id().key() });
                    with_client_seq(&mut frame, client_seq);
                    send(socket, frame).await
                }
                // A removal is terminal — this socket is no longer on the
                // board, so it is told once and dropped. Everything else
                // (locked, full, closed, a bad payload) leaves the room open:
                // a refusal is not a reason to boot a class off the board.
                Err(err @ AppError::Forbidden(_)) => {
                    let _ = send(socket, error_frame(&err, client_seq)).await;
                    Err(RoomClosed)
                }
                Err(err) => send(socket, error_frame(&err, client_seq)).await,
            }
        }
        ClientMessage::Clear => match creator_board(socket, board, user, &st.db).await? {
            None => Ok(()),
            Some(live) => match BoardStroke::clear(live.get_id(), user, &st.db).await {
                // The marker carries the epoch it *closed*; the room's new
                // canvas is the next one. Same shape as `POST /boards/{id}/clear`.
                Ok(marker) => {
                    st.board_hub.publish(
                        board.key(),
                        json!({
                            "type": "cleared",
                            "epoch": marker.get_epoch() + 1,
                            "by": user.key(),
                        })
                        .to_string(),
                    );
                    Ok(())
                }
                Err(err) => send(socket, error_frame(&err, None)).await,
            },
        },
        ClientMessage::Lock { locked } => match creator_board(socket, board, user, &st.db).await? {
            None => Ok(()),
            Some(live) => match live.set_locked(locked, user, &st.db).await {
                Ok(_) => {
                    st.board_hub.publish(
                        board.key(),
                        json!({ "type": "locked", "locked": locked, "by": user.key() }).to_string(),
                    );
                    Ok(())
                }
                Err(err) => send(socket, error_frame(&err, None)).await,
            },
        },
    }
}

/// The board for a creator-only command. `None` means it was refused and the
/// client told — **the socket stays open**: a participant who clicks "clear"
/// has made a mistake, not left the room.
async fn creator_board(
    socket: &mut WebSocket,
    board: &BoardId,
    user: &UserId,
    db: &Database,
) -> Result<Option<Board>, RoomClosed> {
    match live_board(board, user, db).await {
        Ok(live) if live.is_creator(user) => Ok(Some(live)),
        Ok(_) => {
            send(
                socket,
                error_frame(
                    &AppError::Forbidden(
                        "only the board's creator can clear, lock, close or delete it",
                    ),
                    None,
                ),
            )
            .await?;
            Ok(None)
        }
        // Off the board, or the board is gone: both end the room.
        Err(err) => {
            let _ = send(socket, error_frame(&err, None)).await;
            Err(RoomClosed)
        }
    }
}

/// A machine-readable `code` beside the public words. The three refusals the
/// stroke path can raise are distinguished by their message, because that is
/// what [`BoardStroke::append`] hands back and each one means a different thing
/// to a client: `epoch_full` is recoverable by clearing, `locked` is a pause
/// that will lift, `board_closed` is terminal.
fn error_frame(err: &AppError, client_seq: Option<u64>) -> Value {
    let code = match err {
        AppError::Forbidden(_) => "forbidden",
        AppError::Conflict(message) if message.contains("clear it to keep drawing") => "epoch_full",
        AppError::Conflict(message) if message.contains("locked") => "locked",
        // "read-only" (the lifetime cap), and the clear's own "it is closed, or
        // you did not create it" — the socket already checked the creator, so
        // what is left is closed, and that is what the client must act on.
        AppError::Conflict(message)
            if message.contains("read-only") || message.contains("closed") =>
        {
            "board_closed"
        }
        AppError::Conflict(_) | AppError::ConflictOwned(_) => "conflict",
        AppError::Validation(_) | AppError::PayloadTooLarge(_) => "invalid",
        AppError::NotFound => "not_found",
        AppError::Unauthorized => "unauthorized",
        AppError::TooManyRequests { .. } => "too_many_requests",
        AppError::DbUnavailable | AppError::DbTimeout | AppError::Db(_) | AppError::Internal(_) => {
            "internal"
        }
    };
    // The words (and the logging of anything internal) are the rooms' shared
    // half; only the `code` above is this room's own.
    let message = room::public_message(err, ROOM);
    let mut frame = json!({ "type": "error", "code": code, "message": message });
    with_client_seq(&mut frame, client_seq);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three refusals `append` can answer with are one `Conflict` variant
    /// apiece, told apart only by their wording — so this pairing is the whole
    /// contract behind `epoch_full` (clear and carry on), `locked` (wait) and
    /// `board_closed` (never again). Getting it wrong sends a paused room off
    /// clearing a canvas it never needed to lose.
    #[test]
    fn every_refusal_gets_the_code_its_client_must_act_on() {
        let code = |err: AppError| {
            error_frame(&err, None)["code"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            code(AppError::Conflict(
                "this board is full — clear it to keep drawing"
            )),
            "epoch_full"
        );
        assert_eq!(
            code(AppError::Conflict(
                "this board is locked — the creator has paused drawing"
            )),
            "locked"
        );
        assert_eq!(
            code(AppError::Conflict(
                "this board is closed — it is permanently read-only"
            )),
            "board_closed"
        );
        // A stroke that lost its epoch to a clear is none of the three: it is a
        // plain retryable conflict, and must never be mistaken for terminal.
        assert_eq!(
            code(AppError::Conflict(
                "this board changed while you were drawing — draw it again"
            )),
            "conflict"
        );
        assert_eq!(code(AppError::Forbidden("nope")), "forbidden");
        // Internals are logged, never wired.
        let frame = error_frame(&AppError::Internal("secret".into()), None);
        assert_eq!(frame["code"], "internal");
        assert_eq!(frame["message"], "internal server error");
    }

    /// Every refusal the room can raise has to reach the client as the code
    /// that tells it what to *do*: clear (recoverable), wait (paused) or stop
    /// (terminal). The clear's own refusal is the one that hides — it names two
    /// causes, but the socket has already checked the creator, so what is left
    /// is `board_closed` and never the generic `conflict`.
    #[test]
    fn each_refusal_keeps_its_own_code() {
        let code = |err: AppError| {
            error_frame(&err, None)["code"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            code(AppError::Conflict(
                "this board cannot be cleared — it is closed, or you did not create it"
            )),
            "board_closed"
        );
        assert_eq!(
            code(AppError::Conflict(
                "this board is locked — the creator has paused drawing"
            )),
            "locked"
        );
        assert_eq!(
            code(AppError::Conflict(
                "this board is full — clear it to keep drawing"
            )),
            "epoch_full"
        );
    }

    /// Omitted stays omitted, never `null`.
    #[test]
    fn client_seq_rides_along_only_when_it_was_sent() {
        assert_eq!(
            error_frame(&AppError::Forbidden("nope"), Some(9))["client_seq"],
            json!(9)
        );
        assert!(
            error_frame(&AppError::Forbidden("nope"), None)
                .get("client_seq")
                .is_none()
        );
    }
}
