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
use hezarfen_backend::domain::appointment::AppointmentId;
use hezarfen_backend::domain::chatbot_message::{ChatContent, ChatbotMessage};
use hezarfen_backend::domain::chatbot_thread::ChatbotThread;
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
    let thread = ChatbotThread::create_capped(&owner, None, &db)
        .await
        .expect("create thread");
    let id = thread.get_id().clone();
    let say = |text: &str| ChatContent::try_new(text).expect("content");

    // Exactly the order the interleave produces.
    let prompt_a = ChatbotMessage::append_user(&id, &owner, say("soru A"), &db)
        .await
        .expect("prompt A");
    let prompt_b = ChatbotMessage::append_user(&id, &owner, say("soru B"), &db)
        .await
        .expect("prompt B");
    let answer_a = ChatbotMessage::append_pending_assistant(&id, &owner, &db)
        .await
        .expect("answer A");
    let answer_b = ChatbotMessage::append_pending_assistant(&id, &owner, &db)
        .await
        .expect("answer B");

    // The interleave is real: by write order the newest user row before *both*
    // answers is B, which is what used to be handed to both of them.
    let (stored, _) = ChatbotMessage::list_for_thread(&id, None, 0, &db)
        .await
        .expect("thread");
    let texts: Vec<&str> = stored
        .iter()
        .map(|message| message.get_content().as_str())
        .collect();
    assert_eq!(texts, ["soru A", "soru B", "", ""], "the racing row order");

    // Identity decides instead, so each answer keeps its own question — the id
    // the answering task carries is the prompt of its own POST.
    let carried_a = ChatbotMessage::prompt_of(prompt_a.get_id(), &db)
        .await
        .expect("read A")
        .expect("prompt A exists");
    let carried_b = ChatbotMessage::prompt_of(prompt_b.get_id(), &db)
        .await
        .expect("read B")
        .expect("prompt B exists");
    assert_eq!(carried_a.get_content().as_str(), "soru A");
    assert_eq!(carried_b.get_content().as_str(), "soru B");

    // And the answers each settle with the reply to their own question — the
    // stored state, not just what a lookup returned.
    ChatbotMessage::complete(answer_a.get_id(), say("cevap A"), false, &db)
        .await
        .expect("settle A");
    ChatbotMessage::complete(answer_b.get_id(), say("cevap B"), false, &db)
        .await
        .expect("settle B");
    let (stored, _) = ChatbotMessage::list_for_thread(&id, None, 0, &db)
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

/// The overlap check exactly as [`Appointment::conflicts`] asks it, then the
/// approval it authorizes — one `BEGIN…COMMIT`, with a `SLEEP` between the two
/// so both racers read before either writes. Deterministic: the window is wide
/// enough that no scheduling jitter can order the read of one after the write
/// of the other.
const RACING_APPROVE: &str = "\
BEGIN TRANSACTION;
LET $busy = (SELECT VALUE id FROM appointment
  WHERE status = 'approved' AND id != $id AND slot.teacher = $teacher
    AND (IF proposed_starts_at != NONE THEN proposed_starts_at ELSE slot.starts_at END) < $ends
    AND (IF proposed_ends_at != NONE THEN proposed_ends_at ELSE slot.ends_at END) > $starts);
SLEEP 300ms;
IF array::len($busy) = 0 { UPDATE $id SET status = 'approved'; };
COMMIT TRANSACTION;";

/// The same predicate, unraced, as a plain count of what actually committed.
async fn approved_overlapping(
    db: &hezarfen_backend::database::Database,
    teacher: &UserId,
    starts_at: i64,
    ends_at: i64,
) -> usize {
    let mut result = db
        .query(
            "SELECT VALUE id FROM appointment \
             WHERE status = 'approved' AND slot.teacher = $teacher \
               AND (IF proposed_starts_at != NONE THEN proposed_starts_at ELSE slot.starts_at END) < $ends \
               AND (IF proposed_ends_at != NONE THEN proposed_ends_at ELSE slot.ends_at END) > $starts",
        )
        .bind(("teacher", teacher.record()))
        .bind(("starts", starts_at))
        .bind(("ends", ends_at))
        .await
        .expect("count query")
        .check()
        .expect("count query");
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .expect("approved ids")
        .len()
}

/// One racer: the check-then-write above, on its own booking.
async fn race_approve(
    db: hezarfen_backend::database::Database,
    booking: String,
    teacher: UserId,
    starts_at: i64,
    ends_at: i64,
) {
    db.query(RACING_APPROVE)
        .bind(("id", AppointmentId::from_key(&booking).record()))
        .bind(("teacher", teacher.record()))
        .bind(("starts", starts_at))
        .bind(("ends", ends_at))
        .await
        .expect("the racing transaction is sent")
        .check()
        .expect("the racing transaction commits");
}

/// Put the racers' rows back where they started, so the same two transactions
/// can be run twice — once serialized (the control), once raced.
async fn reset_to_pending(db: &hezarfen_backend::database::Database, bookings: &[&str]) {
    for booking in bookings {
        db.query("UPDATE $id SET status = 'pending'")
            .bind(("id", AppointmentId::from_key(booking).record()))
            .await
            .expect("reset")
            .check()
            .expect("reset");
    }
}

/// Publish a slot for `teacher`, book it as `requester`, then counter-propose
/// the shared window onto it — the reachable way two bookings of one teacher
/// come to occupy the same half-hour (different slots, one proposed time).
/// Returns the booking id, left `pending` with the proposal standing.
async fn pending_at(
    app: &axum::Router,
    teacher: &str,
    requester: &str,
    slot: (i64, i64),
    proposed: (i64, i64),
) -> String {
    let booking = booked(app, teacher, requester, slot.0, slot.1).await;
    let res = propose(app, teacher, &booking, proposed.0, proposed.1).await;
    assert_eq!(res.status, StatusCode::OK, "propose: {}", res.body);
    booking
}

/// **This is a property of the store, not a defect in the code above it.**
///
/// "Is this teacher already committed in this window" is a predicate over
/// *other* rows. SurrealDB's `BEGIN…COMMIT` does not conflict-check such a
/// read against a concurrent insert or update, so two transactions can both
/// find the window free and both commit into it — classic write-skew. Nothing
/// in the schema can refuse the second one: there is no single row to key a
/// counter or a compare-and-set on.
///
/// Which is why every approving path takes
/// `domain::appointment::APPOINTMENT_LOCK`, an in-process mutex — it is the
/// *only* thing preventing a double-booked teacher today, and this test is what
/// that claim rests on. The lock is bypassed here on purpose: the racing
/// approvals are sent as raw transactions, so what is measured is the store.
///
/// The Postgres port (decision 2026-09-08) closes this in the database instead:
/// `SERIALIZABLE` would abort the loser of exactly this pattern, and an
/// exclusion constraint over `(teacher, window)` refuses the overlap outright —
/// at which point the lock becomes an optimization rather than the guarantee.
#[tokio::test]
async fn store_does_not_serialize_cross_row_check_then_insert() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let teacher = UserId::from_key(&me_id(&app, &ali).await);
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    // The one half-hour both meetings end up in.
    let window = (now + 10 * HOUR_MS, now + 11 * HOUR_MS);
    let first = pending_at(
        &app,
        &ali,
        &veli,
        (now + HOUR_MS, now + 2 * HOUR_MS),
        window,
    )
    .await;
    let second = pending_at(
        &app,
        &ali,
        &ayse,
        (now + 3 * HOUR_MS, now + 4 * HOUR_MS),
        window,
    )
    .await;
    assert_eq!(
        approved_overlapping(&db, &teacher, window.0, window.1).await,
        0,
        "nothing is committed in that window yet"
    );

    // Control, so the assertion below cannot pass vacuously: run the very same
    // two transactions one after the other and the guard inside them does its
    // job — the second finds the window taken and writes nothing.
    race_approve(
        db.clone(),
        first.clone(),
        teacher.clone(),
        window.0,
        window.1,
    )
    .await;
    race_approve(
        db.clone(),
        second.clone(),
        teacher.clone(),
        window.0,
        window.1,
    )
    .await;
    assert_eq!(
        approved_overlapping(&db, &teacher, window.0, window.1).await,
        1,
        "serialized, the check refuses the second approval"
    );
    reset_to_pending(&db, &[&first, &second]).await;

    // Both check, both find it free, both write, both commit.
    tokio::join!(
        race_approve(db.clone(), first, teacher.clone(), window.0, window.1),
        race_approve(db.clone(), second, teacher.clone(), window.0, window.1),
    );

    let committed = approved_overlapping(&db, &teacher, window.0, window.1).await;
    println!("committed overlapping approved appointments (2 racers): {committed}");
    assert_eq!(
        committed, 2,
        "the store let both check-then-writes commit — this is the write-skew \
         APPOINTMENT_LOCK exists to prevent"
    );
}

/// The same property with eight racers, so the count is not an artifact of a
/// two-way interleaving. At least two commit; in practice all eight do, and the
/// number is printed rather than pinned — the claim is "the store does not
/// serialize this", not "it serializes none of it".
#[tokio::test]
async fn store_does_not_serialize_eight_way_check_then_insert() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let teacher = UserId::from_key(&me_id(&app, &ali).await);
    let now = Timestamp::now().as_millis();
    let window = (now + 30 * HOUR_MS, now + 31 * HOUR_MS);

    let mut bookings = Vec::new();
    for n in 0..8 {
        let requester = login(&app, &format!("veli{n}")).await;
        let slot = (now + (n + 1) * HOUR_MS, now + (n + 2) * HOUR_MS);
        bookings.push(pending_at(&app, &ali, &requester, slot, window).await);
    }

    let mut racers = tokio::task::JoinSet::new();
    for booking in bookings {
        racers.spawn(race_approve(
            db.clone(),
            booking,
            teacher.clone(),
            window.0,
            window.1,
        ));
    }
    while let Some(joined) = racers.join_next().await {
        joined.expect("a racer panicked");
    }

    let committed = approved_overlapping(&db, &teacher, window.0, window.1).await;
    println!("committed overlapping approved appointments (8 racers): {committed}");
    assert!(
        committed >= 2,
        "the store serialized all eight check-then-writes ({committed} committed)"
    );
}
