//! The student exam room: a WebSocket at `GET /exams/{id}/attempt/ws`.
//!
//! REST remains the fallback (`POST /exams/{id}/attempt/answers` etc.); the
//! socket exists so a sitting student gets autosave acks and a live,
//! server-judged countdown without polling. The teacher monitor deliberately
//! stays on SSE (`/exams/{id}/live/stream`) — it only ever flows one way.
//!
//! Wire protocol (JSON text frames):
//!
//! server → client
//! - `{"type":"state", status, attempt, deadline, remaining_ms, now, answered, question_count}`
//!   on connect, every [`EXAM_WS_TICK_SECS`], and after each save
//! - `{"type":"saved", question_id, updated_at}` — an answer landed
//! - `{"type":"pong"}`
//! - `{"type":"finished", finished_at}` then Close — submitted (here or elsewhere)
//! - `{"type":"expired"}` then Close — the deadline passed mid-session
//! - `{"type":"error", message}` — bad JSON, unknown type, validation, deadline
//!
//! client → server
//! - `{"type":"answer", question_id, selected? | text?}`
//! - `{"type":"finish"}`
//! - `{"type":"ping"}`
//!
//! Every save and every tick re-reads the exam and attempt, so the deadline
//! stays server-authoritative: a teacher extending `ends_at` (or `duration_ms`)
//! mid-exam moves this room's clock on the next tick, and a stale client can
//! never write past its real deadline. Every save also re-judges the sitter —
//! live role and enrollment — so a promotion out of `student` or an
//! unenrollment mid-exam closes the sheet with no reconnect needed.
//!
//! The room is also the presence signal behind the rejoin policy: connecting
//! clears the attempt's `left_at`, and the *last* socket of the sitting to
//! close while the attempt is still running stamps it — a student closing one
//! of two tabs hasn't left, and a lingering socket from an already-finished
//! sitting can never mark a later retake as left. With the exam's
//! `allow_rejoin` off, a stamped `left_at` refuses re-entry (and any further
//! saves, here or over REST) until the teacher flips the door back open;
//! finishing stays allowed.
//!
//! Each room is bound to the sitting it was opened for: state frames track
//! that attempt (not whatever is latest), a room whose sitting ends —
//! submitted or expired anywhere — announces it and closes, and `answer` /
//! `finish` refuse to touch any other sitting, so a lingering socket can
//! never write into (or submit) a retake it never hosted.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::constant::EXAM_WS_TICK_SECS;
use crate::database::Database;
use crate::domain::exam::{Exam, ExamId};
use crate::domain::exam_answer::ExamAnswer;
use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt, ExamAttemptId};
use crate::domain::exam_question::ExamQuestion;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::state::AppState;
use crate::web::CurrentUser;
use crate::web::exams::{
    check_rejoin, ensure_enrolled, ensure_student, save_answer_in, writable_attempt,
};

/// What the client asked for, tagged by `type`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientMessage {
    Answer {
        question_id: String,
        #[serde(default)]
        selected: Option<i64>,
        #[serde(default)]
        text: Option<String>,
    },
    Finish,
    Ping,
}

/// Upgrade into the caller's exam room. All gates run *before* the upgrade so
/// a rejected client gets a proper HTTP status instead of an instant close:
/// unknown exam (404), draft with no mode (409), not a student (403), not
/// enrolled (403), no attempt yet (404 — `POST /exams/{id}/attempt` first),
/// submitted or expired (409), left the room while rejoin is closed (409).
/// Entering the room clears the attempt's `left_at` — the student is back
/// inside.
pub async fn attempt_ws(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if exam.get_mode().is_none() {
        return Err(AppError::Conflict(
            "this exam is not scheduled — there is nothing to sit (give it a mode: sync, async, or open)",
        ));
    }
    ensure_student(&user)?;
    ensure_enrolled(&exam, user.get_id(), &st.db).await?;
    let attempt = writable_attempt(&exam, user.get_id(), &st.db).await?;
    check_rejoin(&exam, &attempt)?;
    let attempt = if attempt.get_left_at().is_some() {
        attempt.set_left(None, &st.db).await?
    } else {
        attempt
    };

    let user_id = user.get_id().clone();
    Ok(ws.on_upgrade(move |socket| room(socket, st, exam, attempt, user_id)))
}

/// The room loop: state ticks out, answer/finish/ping in, until the socket
/// closes or the room's sitting reaches a terminal state. The room is bound
/// to the attempt it was opened for — `attempt` — and to no later sitting.
async fn room(mut socket: WebSocket, st: AppState, exam: Exam, attempt: ExamAttempt, user: UserId) {
    let exam_id = exam.get_id().clone();
    let attempt_id = attempt.get_id().clone();
    // This socket counts as presence in the sitting's room until it closes.
    st.exam_presence.enter(attempt_id.key());
    let mut tick = tokio::time::interval(Duration::from_secs(EXAM_WS_TICK_SECS));
    loop {
        tokio::select! {
            // First tick fires immediately — the connect-time state frame.
            _ = tick.tick() => {
                if push_state(&mut socket, &exam_id, &attempt_id, &st.db).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                let text = match incoming {
                    Some(Ok(Message::Text(text))) => text,
                    // Ping/pong frames are answered by axum itself; other
                    // non-text frames carry nothing for this protocol.
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                if handle_message(&mut socket, text.as_str(), &exam_id, &attempt_id, &user, &st.db)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    // Best-effort closing handshake — a bare TCP teardown reads as an error
    // on the client; a Close frame reads as "the room is over".
    let _ = socket.send(Message::Close(None)).await;
    // Only the last socket out means the student actually left the room. If
    // this sitting is still running, stamp the walk-out — with `allow_rejoin`
    // off this is what locks further answering. Terminal exits (finished or
    // expired, and any sitting superseded by a retake is terminal) need no
    // stamp. Best-effort: a failed stamp only means it goes unrecorded.
    if st.exam_presence.leave(attempt_id.key()) {
        stamp_left(&exam_id, &attempt_id, &st.db).await;
    }
}

/// Stamp `left_at` on the room's own sitting if it is still in progress —
/// re-reading both rows so a finish or expiry that raced the socket close
/// wins. Targeting the sitting by id (never "the latest") means a retake
/// started elsewhere can't be marked as left by an old room's teardown.
async fn stamp_left(exam_id: &ExamId, attempt_id: &ExamAttemptId, db: &Database) {
    let attempt = match Exam::read(exam_id, db).await {
        Ok(Some(exam)) => match ExamAttempt::read(attempt_id, db).await {
            Ok(Some(attempt))
                if attempt.status(&exam, Timestamp::now()) == AttemptStatus::InProgress =>
            {
                Some(attempt)
            }
            Ok(_) => None,
            Err(err) => {
                tracing::warn!("exam room could not read the attempt to stamp left_at: {err}");
                None
            }
        },
        Ok(None) => None,
        Err(err) => {
            tracing::warn!("exam room could not read the exam to stamp left_at: {err}");
            None
        }
    };
    if let Some(attempt) = attempt
        && let Err(err) = attempt.set_left(Some(Timestamp::now()), db).await
    {
        tracing::warn!("exam room could not stamp left_at: {err}");
    }
}

/// Errors that end the room: the peer went away, or the attempt reached a
/// terminal state and the close frame was sent.
struct RoomClosed;

/// The room's own sitting, provided it is still the student's current one and
/// writable. Messages act on the sitting the room was opened for — never on a
/// retake started elsewhere while this socket lingered, which a stale tab
/// could otherwise scribble on or instantly submit (burning a limited
/// sitting). The latest-attempt read also keeps the deadline gates on the
/// exam's current schedule, exactly like the REST path.
async fn writable_room_attempt(
    exam: &Exam,
    attempt_id: &ExamAttemptId,
    user: &UserId,
    db: &Database,
) -> Result<ExamAttempt, AppError> {
    let attempt = writable_attempt(exam, user, db).await?;
    if attempt.get_id() != attempt_id {
        return Err(AppError::Conflict(
            "this room's sitting is over — reconnect to continue in the new attempt",
        ));
    }
    Ok(attempt)
}

async fn send(socket: &mut WebSocket, frame: Value) -> Result<(), RoomClosed> {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|_| RoomClosed)
}

/// Re-read everything, push a `state` frame, and close the room (after a
/// `finished`/`expired` notice) once the room's sitting is no longer in
/// progress.
async fn push_state(
    socket: &mut WebSocket,
    exam: &ExamId,
    attempt: &ExamAttemptId,
    db: &Database,
) -> Result<(), RoomClosed> {
    let (frame, status, finished_at) = match state_frame(exam, attempt, db).await {
        Ok(state) => state,
        // The exam vanished mid-room (deleted) or the db hiccuped: nothing
        // sensible left to serve.
        Err(err) => {
            tracing::warn!("exam room state failed: {err}");
            let _ = send(
                socket,
                json!({ "type": "error", "message": "state unavailable" }),
            )
            .await;
            return Err(RoomClosed);
        }
    };
    send(socket, frame).await?;
    match status {
        AttemptStatus::InProgress => Ok(()),
        AttemptStatus::Submitted => {
            send(
                socket,
                json!({ "type": "finished", "finished_at": finished_at }),
            )
            .await?;
            Err(RoomClosed)
        }
        AttemptStatus::Expired => {
            send(socket, json!({ "type": "expired" })).await?;
            Err(RoomClosed)
        }
    }
}

/// One server-judged snapshot of the room's own sitting, shaped like the REST
/// `AttemptResponse`'s live fields.
async fn state_frame(
    exam: &ExamId,
    attempt: &ExamAttemptId,
    db: &Database,
) -> Result<(Value, AttemptStatus, Option<i64>), AppError> {
    let exam = Exam::read(exam, db).await?.ok_or(AppError::NotFound)?;
    let attempt = ExamAttempt::read(attempt, db)
        .await?
        .ok_or(AppError::NotFound)?;
    let now = Timestamp::now();
    let status = attempt.status(&exam, now);
    let deadline = attempt.deadline(&exam);
    let remaining_ms = (status == AttemptStatus::InProgress)
        .then(|| deadline.map(|d| (d.as_millis() - now.as_millis()).max(0)))
        .flatten();
    let answered = ExamAnswer::list_for_exam_user(exam.get_id(), attempt.get_user(), db)
        .await?
        .len();
    let question_count = ExamQuestion::list_for_exam(exam.get_id(), db).await?.len();
    let frame = json!({
        "type": "state",
        "status": status.as_str(),
        "attempt": attempt.get_seq(),
        "deadline": deadline.map(|t| t.as_millis()),
        "remaining_ms": remaining_ms,
        "now": now.as_millis(),
        "answered": answered,
        "question_count": question_count,
    });
    Ok((
        frame,
        status,
        attempt.get_finished_at().map(|t| t.as_millis()),
    ))
}

async fn handle_message(
    socket: &mut WebSocket,
    text: &str,
    exam_id: &ExamId,
    attempt_id: &ExamAttemptId,
    user: &UserId,
    db: &Database,
) -> Result<(), RoomClosed> {
    let message = match serde_json::from_str::<ClientMessage>(text) {
        Ok(message) => message,
        Err(err) => {
            return send(
                socket,
                json!({ "type": "error", "message": format!("unrecognized message: {err}") }),
            )
            .await;
        }
    };
    match message {
        ClientMessage::Ping => send(socket, json!({ "type": "pong" })).await,
        ClientMessage::Answer {
            question_id,
            selected,
            text,
        } => {
            // Re-read the exam so the save is judged against the *current*
            // schedule, exactly like the REST path it shares — but write into
            // the room's own sitting, never whatever is latest.
            let saved = match Exam::read(exam_id, db).await {
                Ok(Some(exam)) => match writable_room_attempt(&exam, attempt_id, user, db).await {
                    Ok(attempt) => {
                        save_answer_in(&exam, &attempt, &question_id, selected, text, db).await
                    }
                    Err(err) => Err(err),
                },
                Ok(None) => Err(AppError::NotFound),
                Err(err) => Err(err),
            };
            match saved {
                Ok(answer) => {
                    send(
                        socket,
                        json!({
                            "type": "saved",
                            "question_id": answer.get_question().key(),
                            "updated_at": answer.get_updated_at().as_millis(),
                        }),
                    )
                    .await?;
                    // Progress changed — refresh the countdown/answered state
                    // right away rather than waiting out the tick.
                    push_state(socket, exam_id, attempt_id, db).await
                }
                Err(err) => send(socket, error_frame(&err)).await,
            }
        }
        ClientMessage::Finish => {
            // Submit the room's own sitting — a stale room must not submit a
            // retake it never hosted.
            let finished = match Exam::read(exam_id, db).await {
                Ok(Some(exam)) => match writable_room_attempt(&exam, attempt_id, user, db).await {
                    Ok(attempt) => attempt.finish(db).await,
                    Err(err) => Err(err),
                },
                Ok(None) => Err(AppError::NotFound),
                Err(err) => Err(err),
            };
            match finished {
                Ok(attempt) => {
                    send(
                        socket,
                        json!({
                            "type": "finished",
                            "finished_at": attempt.get_finished_at().map(|t| t.as_millis()),
                        }),
                    )
                    .await?;
                    Err(RoomClosed)
                }
                Err(err) => send(socket, error_frame(&err)).await,
            }
        }
    }
}

/// The public words for an error — the same strings the HTTP layer would use,
/// with internals logged, never sent.
fn error_frame(err: &AppError) -> Value {
    let message = match err {
        AppError::Validation(err) => err.to_string(),
        AppError::NotFound => "not found".to_string(),
        AppError::Unauthorized => "unauthorized".to_string(),
        AppError::Forbidden(message) => (*message).to_string(),
        AppError::Conflict(message) => (*message).to_string(),
        AppError::TooManyRequests { .. } => "too many requests".to_string(),
        AppError::Db(_) | AppError::Internal(_) => {
            tracing::error!("exam room error: {err}");
            "internal server error".to_string()
        }
    };
    json!({ "type": "error", "message": message })
}
