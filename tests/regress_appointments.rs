//! Regressions for two defects found in the 2026-08-02 sweep:
//!
//! - a teacher could confirm their **own** counter-proposal through
//!   `PATCH /appointments/{id}/approve`, moving a meeting the requester had
//!   already agreed to onto a time only the teacher ever named;
//! - a chatbot answer resolved its question by *write order*, so two POSTs on
//!   one thread interleaving across their two row creates made both answers
//!   reply to the second prompt and left the first unanswered.
//!
//! The second is a true race, so the reachable half is driven: the row order
//! the interleave produces is written straight into the store, and the
//! assertion is on what the store then pairs.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, id_of, login, login_as, me_id, send};
use hezarfen_backend::db::{chatbot_message, chatbot_thread};
use hezarfen_backend::domain::chatbot_message::ChatContent;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use serde_json::json;

const HOUR_MS: i64 = 3_600_000;

/// `PATCH /appointments/{id}/{action}`, bodyless.
async fn decide(app: &axum::Router, cookie: &str, id: &str, action: &str) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/{action}"),
        Some(cookie),
        None,
    )
    .await
}

/// `PATCH /appointments/{id}/reschedule/accept`, naming the proposal accepted.
async fn accept(
    app: &axum::Router,
    cookie: &str,
    id: &str,
    starts_at: i64,
    ends_at: i64,
) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/reschedule/accept"),
        Some(cookie),
        Some(json!({ "proposed_starts_at": starts_at, "proposed_ends_at": ends_at })),
    )
    .await
}

/// `PATCH /appointments/{id}/reschedule` — the teacher counter-proposes.
async fn propose(
    app: &axum::Router,
    cookie: &str,
    id: &str,
    starts_at: i64,
    ends_at: i64,
) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/reschedule"),
        Some(cookie),
        Some(json!({ "starts_at": starts_at, "ends_at": ends_at })),
    )
    .await
}

/// Publish one slot as `cookie` and book it as `requester`; returns the
/// booking id.
async fn booked(
    app: &axum::Router,
    teacher: &str,
    requester: &str,
    starts_at: i64,
    ends_at: i64,
) -> String {
    let res = send(
        app,
        "POST",
        "/appointments/slots",
        Some(teacher),
        Some(json!({ "starts_at": starts_at, "ends_at": ends_at })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "publish: {}", res.body);
    let slot = res.body[0]["id"].as_str().expect("slot id").to_string();

    let res = send(
        app,
        "POST",
        "/appointments",
        Some(requester),
        Some(json!({ "slot": slot, "reason": "görüşmek istiyorum" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "book: {}", res.body);
    id_of(&res.body)
}

/// A confirmed meeting may only move if the person who asked for it says so.
/// The teacher's own approval used to be enough: `reschedule` puts the booking
/// back to `pending` with the proposal on the row, and `approve` then read the
/// *effective* window — the proposal's — without ever asking whether one was
/// standing. That committed the requester to a time they never saw, straight
/// past the requester-only `reschedule/accept` gate.
#[tokio::test]
async fn a_teacher_cannot_confirm_their_own_counter_proposal() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let (slot_starts_at, slot_ends_at) = (now + HOUR_MS, now + 2 * HOUR_MS);
    let (moved_starts_at, moved_ends_at) = (now + 3 * HOUR_MS, now + 4 * HOUR_MS);
    let booking = booked(&app, &ali, &veli, slot_starts_at, slot_ends_at).await;

    // Agreed: the meeting is committed at the slot's own window.
    let res = decide(&app, &ali, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["starts_at"], slot_starts_at);

    // The teacher counter-proposes: back to pending, nothing committed yet.
    let res = propose(&app, &ali, &booking, moved_starts_at, moved_ends_at).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    assert_eq!(res.body["proposed_starts_at"], moved_starts_at);

    // And now approves their own proposal. This is the defect: it used to
    // answer 200 and commit the meeting at the moved time.
    let res = decide(&app, &ali, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"]
            .as_str()
            .expect("error text")
            .contains("counter-proposal"),
        "{}",
        res.body
    );

    // Refused, not half-applied: still pending, the proposal still standing and
    // still unanswered.
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    let row = &common::items(&res.body)[0];
    assert_eq!(row["id"], booking.as_str());
    assert_eq!(row["status"], "pending");
    assert_eq!(row["proposed_starts_at"], moved_starts_at);
    assert!(row["decided_by"].is_null(), "nothing was decided");

    // The requester's own accept is still the way through, and it commits the
    // proposed time — the gate refuses the teacher, not the move.
    let res = accept(&app, &veli, &booking, moved_starts_at, moved_ends_at).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["starts_at"], moved_starts_at);
    assert_eq!(res.body["ends_at"], moved_ends_at);
}

/// The other half of the same invariant: the requester must be committed to the
/// time *they saw*. `propose` takes no lock and the requester answers minutes
/// later, so a teacher who re-proposes in between used to choose which window
/// the accept landed on — the click said 10:00 and the row said 23:00. The
/// accept names the proposal it answers, so a superseded one is refused.
#[tokio::test]
async fn accepting_a_superseded_proposal_is_refused() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let booking = booked(&app, &ali, &veli, now + HOUR_MS, now + 2 * HOUR_MS).await;

    // The proposal the requester reads and agrees to.
    let (seen_starts_at, seen_ends_at) = (now + 3 * HOUR_MS, now + 4 * HOUR_MS);
    let res = propose(&app, &ali, &booking, seen_starts_at, seen_ends_at).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(
        common::items(&res.body)[0]["proposed_starts_at"],
        seen_starts_at
    );

    // The teacher swaps it out while the requester is deciding.
    let (swapped_starts_at, swapped_ends_at) = (now + 23 * HOUR_MS, now + 24 * HOUR_MS);
    let res = propose(&app, &ali, &booking, swapped_starts_at, swapped_ends_at).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The click, carrying what they saw. It used to approve at the swapped time.
    let res = accept(&app, &veli, &booking, seen_starts_at, seen_ends_at).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"]
            .as_str()
            .expect("error text")
            .contains("proposed time has changed"),
        "{}",
        res.body
    );

    // Nothing was committed, at either time.
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    let row = &common::items(&res.body)[0];
    assert_eq!(row["status"], "pending");
    assert_eq!(row["proposed_starts_at"], swapped_starts_at);
    assert!(row["decided_by"].is_null());

    // Re-reading and accepting what actually stands still works — the pin
    // refuses a stale answer, not the move.
    let res = accept(&app, &veli, &booking, swapped_starts_at, swapped_ends_at).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["starts_at"], swapped_starts_at);
    assert_eq!(res.body["ends_at"], swapped_ends_at);
}

/// Not a defect — a pin on a deliberate disclosure, so a future "tightening"
/// fails loudly instead of silently killing the booking flow. `GET
/// /appointments/slots` hands a parent the slot teacher's `PersonRef`, an
/// identity a parent's direct routes still refuse them (`/users/{id}/profile`
/// 403, `/users` admin) — `/users/search` names staff since #24, but never says
/// which of them published bookable time. It has to: you cannot choose whom to
/// book a conference with from an anonymous calendar.
#[tokio::test]
async fn a_parent_reads_the_slot_teachers_identity() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login_as(&app, &db, "veli", "parent").await;
    let now = Timestamp::now().as_millis();

    let res = send(
        &app,
        "POST",
        "/appointments/slots",
        Some(&ali),
        Some(json!({ "starts_at": now + HOUR_MS, "ends_at": now + 2 * HOUR_MS })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The parent is refused that identity everywhere it is asked for directly.
    for uri in [format!("/users/{ali_id}/profile"), "/users".to_string()] {
        let res = send(&app, "GET", &uri, Some(&veli), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri}: {}", res.body);
    }

    // And gets it here anyway, on purpose: id, username, and the name they need
    // to recognise the teacher they are booking.
    let res = send(&app, "GET", "/appointments/slots", Some(&veli), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let teacher = &common::items(&res.body)[0]["teacher"];
    assert_eq!(teacher["id"], ali_id.as_str(), "{}", res.body);
    assert_eq!(teacher["username"], "ali", "{}", res.body);
    assert!(teacher.get("display_name").is_some(), "{}", res.body);
}

/// An answer belongs to the question of *its own* POST. Two POSTs on one thread
/// interleave across `append_user` / `append_pending_assistant`, so the rows
/// land `userA, userB, asstA, asstB` — and "the newest user row written before
/// this answer", the rule that used to pair them, resolves asstA to prompt B.
/// Both answers then reply to B and A is never answered. The pairing is carried
/// by id now, so write order cannot decide it.
#[tokio::test]
async fn an_answer_resolves_its_own_prompt_across_an_interleave() {
    let (app, db) = app_and_db().await;
    let cookie = login(&app, "veli").await;
    let owner = UserId::from_key(&me_id(&app, &cookie).await);
    let thread = chatbot_thread::create_capped(&db, &owner, None)
        .await
        .expect("create thread");
    let id = thread.get_id().clone();
    let say = |text: &str| ChatContent::try_new(text).expect("content");

    // Exactly the order the interleave produces.
    let prompt_a = chatbot_message::append_user(&db, &id, &owner, say("soru A"))
        .await
        .expect("prompt A");
    let prompt_b = chatbot_message::append_user(&db, &id, &owner, say("soru B"))
        .await
        .expect("prompt B");
    let answer_a = chatbot_message::append_pending_assistant(&db, &id, &owner)
        .await
        .expect("answer A");
    let answer_b = chatbot_message::append_pending_assistant(&db, &id, &owner)
        .await
        .expect("answer B");

    // The interleave is real: by write order the newest user row before *both*
    // answers is B, which is what used to be handed to both of them.
    let (stored, _) = chatbot_message::list_for_thread(&db, &id, None, 0)
        .await
        .expect("thread");
    let texts: Vec<&str> = stored
        .iter()
        .map(|message| message.get_content().as_str())
        .collect();
    assert_eq!(texts, ["soru A", "soru B", "", ""], "the racing row order");

    // Identity decides instead, so each answer keeps its own question — the id
    // the answering task carries is the prompt of its own POST.
    let carried_a = chatbot_message::prompt_of(&db, prompt_a.get_id())
        .await
        .expect("read A")
        .expect("prompt A exists");
    let carried_b = chatbot_message::prompt_of(&db, prompt_b.get_id())
        .await
        .expect("read B")
        .expect("prompt B exists");
    assert_eq!(carried_a.get_content().as_str(), "soru A");
    assert_eq!(carried_b.get_content().as_str(), "soru B");

    // And the answers each settle with the reply to their own question — the
    // stored state, not just what a lookup returned.
    chatbot_message::complete(&db, answer_a.get_id(), say("cevap A"), false)
        .await
        .expect("settle A");
    chatbot_message::complete(&db, answer_b.get_id(), say("cevap B"), false)
        .await
        .expect("settle B");
    let (stored, _) = chatbot_message::list_for_thread(&db, &id, None, 0)
        .await
        .expect("thread");
    let texts: Vec<&str> = stored
        .iter()
        .map(|message| message.get_content().as_str())
        .collect();
    assert_eq!(texts, ["soru A", "soru B", "cevap A", "cevap B"]);
}

// ---------------------------------------------------------------------------
// The store property behind `APPOINTMENT_LOCK`.
// ---------------------------------------------------------------------------

