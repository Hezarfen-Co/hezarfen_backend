//! The student exam room: a WebSocket at `GET /exams/{id}/attempt/ws`.
//!
//! REST remains the fallback (`POST /exams/{id}/attempt/answers` etc.); the
//! socket exists so a sitting student gets autosave acks and a live,
//! server-judged countdown without polling. The teacher monitor polls the
//! one-shot snapshot (`GET /exams/{id}/live`) instead.
//!
//! Wire protocol (JSON text frames):
//!
//! server → client
//! - `{"type":"state", status, attempt, deadline, remaining_ms, now, answered, question_count}`
//!   on connect, every [`EXAM_WS_TICK_SECS`], and after each save
//! - `{"type":"saved", question_id, updated_at, client_seq?}` — an answer landed
//! - `{"type":"pong"}`
//! - `{"type":"finished", finished_at}` then Close — submitted (here or elsewhere)
//! - `{"type":"expired"}` then Close — the deadline passed mid-session
//! - `{"type":"error", message, question_id?, client_seq?}` — bad JSON, unknown type,
//!   validation, deadline. `question_id` is present only when the failure
//!   belongs to that one `answer` (its payload or its question); absent means
//!   the failure is connection- or sitting-level, so every save in flight is
//!   equally refused
//!
//! `client_seq` on either reply is whatever the `answer` sent, echoed verbatim: the
//! server never reads it, never dedupes on it, and omits the key entirely when
//! the request omitted it. `question_id` cannot serve as the correlation id —
//! a re-save of the same question after a timeout leaves two sends
//! outstanding for it.
//!
//! client → server
//! - `{"type":"answer", question_id, selected? | text?, client_seq?}` (`selected` =
//!   choice id, `client_seq` = the client's own correlation id)
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
//! The room is also the presence signal behind the rejoin policy: joining the
//! room clears the attempt's `left_at`, and the *last* socket of the sitting to
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

use crate::constant::{EXAM_WS_TICK_SECS, MAX_QUESTION_ID_LEN};
use crate::database::Database;
use crate::domain::exam::{Exam, ExamId};
use crate::domain::exam_answer::ExamAnswer;
use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt, ExamAttemptId};
use crate::domain::exam_question::ExamQuestion;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::state::AppState;
use crate::validate::validate_required;
use crate::web::CurrentUser;
use crate::web::exams::{
    EXAM_LOCK, check_rejoin, ensure_enrolled, ensure_sittable, ensure_student, save_answer_in,
    writable_attempt,
};

/// Serializes every presence transition with its matching `left_at` write:
/// `enter` + clear at room start and `leave` + maybe-stamp at room teardown
/// are each one critical section, so any join and any teardown run wholly
/// before or wholly after each other. Teardown first: its stamp lands, then
/// the join's clear overwrites it — the student is present and unmarked.
/// Join first: the teardown's `leave` sees the joiner's socket still counted,
/// so it never stamps. No ordering leaves a present student stamped as left —
/// the reconnect-vs-teardown race that used to lock students out with
/// `allow_rejoin` off (cleared at the door, stamped after, present forever
/// refused). This backend is the database's only writer (single
/// instance) — so one process-wide lock is sufficient, same as `REGISTER_LOCK`.
// ponytail: global lock, per-attempt locks if room churn ever shows up in a
// profile.
static PRESENCE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What the client asked for, tagged by `type`.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientMessage {
    Answer {
        question_id: String,
        #[serde(default)]
        selected: Option<String>,
        #[serde(default)]
        text: Option<String>,
        /// The client's own correlation id, echoed verbatim on this message's
        /// `saved` or `error` and never read by the server. `question_id` is
        /// not an identity — a re-save of the same question after a timeout
        /// has two sends outstanding, and the first reply would otherwise
        /// settle the second.
        #[serde(default)]
        client_seq: Option<u64>,
    },
    Finish,
    Ping,
}

/// Upgrade into the caller's exam room. All gates run *before* the upgrade so
/// a rejected client gets a proper HTTP status instead of an instant close:
/// unknown or draft exam (404), unscheduled with no mode (409), not a student
/// (403), not enrolled (403), no attempt yet (404 — `POST /exams/{id}/attempt`
/// first), submitted or expired (409), left the room while rejoin is closed
/// (409).
///
/// The rejoin gate here is a read-only fast-fail for a proper 409; the
/// authoritative clear of `left_at` happens inside the room task, under
/// [`PRESENCE_LOCK`], atomically with the presence count — milliseconds after
/// this gate, which no HTTP caller can observe. Clearing it here instead
/// (before the socket counts as present) is exactly the ordering that let a
/// dying socket's teardown stamp a student who was already reconnecting.
pub async fn attempt_ws(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    let exam = Exam::read(&ExamId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_sittable(&exam)?;
    ensure_student(&user)?;
    ensure_enrolled(&exam, user.get_id(), &st.db).await?;
    let attempt = writable_attempt(&exam, user.get_id(), &st.db).await?;
    check_rejoin(&exam, &attempt)?;

    let user_id = user.get_id().clone();
    Ok(ws.on_upgrade(move |socket| room(socket, st, exam, attempt, user_id)))
}

/// The room loop: state ticks out, answer/finish/ping in, until the socket
/// closes or the room's sitting reaches a terminal state. The room is bound
/// to the attempt it was opened for — `attempt` — and to no later sitting.
async fn room(mut socket: WebSocket, st: AppState, exam: Exam, attempt: ExamAttempt, user: UserId) {
    let exam_id = exam.get_id().clone();
    let attempt_id = attempt.get_id().clone();
    // Join critical section: this socket counts as presence in the sitting's
    // room until it closes, and joining clears the walk-out marker — one
    // atomic step under PRESENCE_LOCK, so a dying socket's teardown either
    // stamps before this (and the clear overwrites it) or sees this socket
    // counted (and never stamps). The clear is unconditional: the door's
    // snapshot may predate a stamp that raced the upgrade. Best-effort — a
    // failed clear leaves the stamp for the next join or the teacher's door.
    {
        let _guard = PRESENCE_LOCK.lock().await;
        st.exam_presence.enter(attempt_id.key());
        if let Err(err) = attempt.set_left(None, &st.db).await {
            tracing::warn!("exam room could not clear left_at on join: {err}");
        }
    }
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
    // Leave critical section: only the last socket out means the student
    // actually left the room, and the count-down and its stamp are one atomic
    // step under PRESENCE_LOCK (a join racing this either lands wholly before
    // — its socket keeps the count up, no stamp — or wholly after, clearing
    // whatever this stamps). If this sitting is still running, stamp the
    // walk-out — with `allow_rejoin` off this is what locks further
    // answering. Terminal exits (finished or expired, and any sitting
    // superseded by a retake is terminal) need no stamp. Best-effort: a
    // failed stamp only means it goes unrecorded.
    let _guard = PRESENCE_LOCK.lock().await;
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
    let answered =
        ExamAnswer::list_for_exam_user(exam.get_id(), attempt.get_user(), attempt.get_seq(), db)
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
            client_seq,
        } => {
            // Cap the key before it can be echoed — see [`MAX_QUESTION_ID_LEN`].
            if let Err(err) = validate_required("question_id", &question_id, MAX_QUESTION_ID_LEN) {
                let frame = error_frame_for(&AppError::Validation(err), None, client_seq);
                return send(socket, frame).await;
            }
            // Re-read the exam so the save is judged against the *current*
            // schedule, exactly like the REST path it shares — but write into
            // the room's own sitting, never whatever is latest. Reader lease
            // of [`EXAM_LOCK`] from the gates through the upsert, exactly
            // like `save_answer_checked` — and dropped before the socket
            // sends, so a slow client never stalls a writer.
            let guard = EXAM_LOCK.read().await;
            // Tracks how far the gates got: only once the sitting resolved can
            // a failure possibly be about this one question rather than about
            // the room. See [`error_frame_for`].
            let mut in_save = false;
            let saved = match Exam::read(exam_id, db).await {
                Ok(Some(exam)) => match writable_room_attempt(&exam, attempt_id, user, db).await {
                    Ok(attempt) => {
                        in_save = true;
                        save_answer_in(&exam, &attempt, &question_id, selected, text, db).await
                    }
                    Err(err) => Err(err),
                },
                Ok(None) => Err(AppError::NotFound),
                Err(err) => Err(err),
            };
            drop(guard);
            match saved {
                Ok(answer) => {
                    let mut frame = json!({
                        "type": "saved",
                        "question_id": answer.get_question().key(),
                        "updated_at": answer.get_updated_at().as_millis(),
                    });
                    with_client_seq(&mut frame, client_seq);
                    send(socket, frame).await?;
                    // Progress changed — refresh the countdown/answered state
                    // right away rather than waiting out the tick.
                    push_state(socket, exam_id, attempt_id, db).await
                }
                Err(err) => {
                    let attributed = in_save && attributable(&err);
                    send(
                        socket,
                        error_frame_for(
                            &err,
                            attributed.then_some(question_id.as_str()),
                            client_seq,
                        ),
                    )
                    .await
                }
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

/// Whether a failure raised *inside* the save (past the exam and sitting
/// gates) is about the answered question alone, so the client can fail that
/// one save instead of every save in flight.
///
/// Only the two question-shaped failures qualify: [`AppError::Validation`]
/// (the payload doesn't fit the question — wrong kind, unknown choice, too
/// long) and [`AppError::NotFound`] (no such question in this exam). Every
/// other variant from the save path is about the sitter or the sitting —
/// `Unauthorized`/`Forbidden` (promoted out of `student`, unenrolled),
/// `Conflict` (walked out with rejoin closed) — and dooms the next save just
/// as much as this one, so it stays unattributed and keeps fail-everything.
/// Db/Internal stay unattributed too: a database that just failed is not a
/// per-question fact, and over-reporting there is the safe side.
fn attributable(err: &AppError) -> bool {
    matches!(err, AppError::Validation(_) | AppError::NotFound)
}

/// The public words for an error — the same strings the HTTP layer would use,
/// with internals logged, never sent.
fn error_frame(err: &AppError) -> Value {
    error_frame_for(err, None, None)
}

/// Attach the request's correlation id, if it sent one. Omitted stays omitted
/// — never `null` — so a client that sends no `client_seq` sees byte-identical
/// frames.
fn with_client_seq(frame: &mut Value, client_seq: Option<u64>) {
    if let Some(client_seq) = client_seq {
        frame["client_seq"] = json!(client_seq);
    }
}

/// [`error_frame`] plus the optional blame: `question_id` when the failure
/// belongs to one `answer` message, and `client_seq` whenever the `answer` carried
/// one — the two are independent, so an unattributable failure is still
/// matchable to the send that caused it. Absent keeps its original meaning —
/// an unattributed failure — so a client that ignores the fields behaves
/// exactly as before.
fn error_frame_for(err: &AppError, question: Option<&str>, client_seq: Option<u64>) -> Value {
    let message = match err {
        AppError::Validation(err) => err.to_string(),
        AppError::NotFound => "not found".to_string(),
        AppError::Unauthorized => "unauthorized".to_string(),
        AppError::Forbidden(message) => (*message).to_string(),
        AppError::Conflict(message) => (*message).to_string(),
        AppError::ConflictOwned(message) => message.clone(),
        AppError::PayloadTooLarge(message) => message.clone(),
        AppError::TooManyRequests { .. } => "too many requests".to_string(),
        AppError::DbUnavailable => {
            tracing::warn!("exam room: database reconnecting");
            "database reconnecting — retry shortly".to_string()
        }
        AppError::DbTimeout => {
            tracing::error!("exam room: database timed out");
            "the database timed out — reload the room".to_string()
        }
        AppError::Db(_) | AppError::Internal(_) => {
            tracing::error!("exam room error: {err}");
            "internal server error".to_string()
        }
    };
    let mut frame = json!({ "type": "error", "message": message });
    if let Some(question) = question {
        frame["question_id"] = json!(question);
    }
    with_client_seq(&mut frame, client_seq);
    frame
}
