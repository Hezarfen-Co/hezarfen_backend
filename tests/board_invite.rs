//! `POST /boards/{id}/invite` — filling a whiteboard's roster from a group that
//! already exists (a class section, a course/club, an event's roster) instead of
//! one id at a time.
//!
//! The invariants pinned here are the ones the feature would be wrong without:
//! the expansion is a **snapshot** and not a subscription, it only ever **adds**,
//! the participant cap is **all-or-nothing**, ineligible members are dropped
//! **silently** while the whole call still succeeds, and — because a board's
//! roster is visible to everyone on the board — each source carries the gate its
//! own listing route carries, so bulk invite cannot become a roster-disclosure
//! oracle for a student.

mod common;

use axum::http::StatusCode;
use common::{Res, app_and_db, create_course, enroll, login, login_as, me_id, send};
use hezarfen_backend::constant::MAX_BOARD_PARTICIPANTS;
use hezarfen_backend::domain::board::BoardId;
use hezarfen_backend::domain::user::UserId;
use serde_json::{Value, json};

/// Open a board as `cookie`; returns its id.
async fn a_board(app: &axum::Router, cookie: &str) -> String {
    let res = send(
        app,
        "POST",
        "/boards",
        Some(cookie),
        Some(json!({ "title": "Tahta" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    res.body["id"].as_str().unwrap().to_string()
}

async fn invite(app: &axum::Router, cookie: &str, board: &str, source: Value) -> Res {
    send(
        app,
        "POST",
        &format!("/boards/{board}/invite"),
        Some(cookie),
        Some(source),
    )
    .await
}

/// The roster as the *server* holds it, read back off `GET /boards/{id}` rather
/// than trusted from the invite's own reply.
async fn roster(app: &axum::Router, cookie: &str, board: &str) -> Vec<String> {
    let res = send(app, "GET", &format!("/boards/{board}"), Some(cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mut ids: Vec<String> = res.body["participants"]
        .as_array()
        .expect("participants array")
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// A class section with `members` in it, created by `manager`.
async fn a_class(app: &axum::Router, manager: &str, name: &str, members: &[&str]) -> String {
    let res = send(
        app,
        "POST",
        "/classes",
        Some(manager),
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let class = res.body["id"].as_str().unwrap().to_string();
    for user in members {
        let res = send(
            app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(manager),
            Some(json!({ "user_id": user })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    class
}

/// The headline case: a teacher fills a board with a whole class in one call,
/// and the expansion is a snapshot — a student who joins the class *after* the
/// invite is not on the board until someone invites the class again, and that
/// re-invite adds only the person who was missing.
#[tokio::test]
async fn a_class_invite_fills_the_roster_and_re_inviting_tops_it_up() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager_a", "manager").await;
    let teacher = login_as(&app, &db, "teacher_a", "teacher").await;
    let ali = login(&app, "ali").await;
    let ayse = login(&app, "ayse").await;
    let deniz = login(&app, "deniz").await;
    let (ali_id, ayse_id, deniz_id) = (
        me_id(&app, &ali).await,
        me_id(&app, &ayse).await,
        me_id(&app, &deniz).await,
    );

    let class = a_class(&app, &manager, "9-A", &[&ali_id, &ayse_id]).await;
    let board = a_board(&app, &teacher).await;

    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "class", "class": class}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let mut expected = vec![ali_id.clone(), ayse_id.clone()];
    expected.sort();
    assert_eq!(
        roster(&app, &teacher, &board).await,
        expected,
        "the whole class should be on the board"
    );

    // The class gains a student. The board is a snapshot, so nothing happens to
    // it — this is the assertion that separates this design from a live roster.
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": deniz_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(
        roster(&app, &teacher, &board).await,
        expected,
        "a snapshot must not follow the class; the new member appeared on the board by itself"
    );

    // Re-inviting the same source tops it up, adding only who is missing.
    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "class", "class": class}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mut topped = vec![ali_id, ayse_id, deniz_id];
    topped.sort();
    assert_eq!(
        roster(&app, &teacher, &board).await,
        topped,
        "a re-invite should add the missing member and duplicate nobody"
    );
}

/// "Invite the whole club" is the course form — a club is a course whose `kind`
/// is `club`, so the club case needs no source of its own. The same call also
/// pins the two things an invite must never do: remove anybody, or spend a seat
/// on the creator (who is a participant by construction and never sits in the
/// array).
#[tokio::test]
async fn a_club_invite_adds_its_members_without_removing_a_hand_invited_one() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher_b", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let ali = login(&app, "ali").await;
    let ayse = login(&app, "ayse").await;
    let hakan = login(&app, "hakan").await;
    let (ali_id, ayse_id, hakan_id) = (
        me_id(&app, &ali).await,
        me_id(&app, &ayse).await,
        me_id(&app, &hakan).await,
    );

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "Satranç Kulübü", "kind": "club" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let club = res.body["id"].as_str().unwrap().to_string();
    enroll(&app, &teacher, &club, &ali_id).await;
    enroll(&app, &teacher, &club, &ayse_id).await;

    // Hakan is not in the club — he was invited by hand, and the club invite
    // must leave him alone.
    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&teacher),
        Some(json!({ "title": "Açılış", "participants": [hakan_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let board = res.body["id"].as_str().unwrap().to_string();

    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "course", "course": club}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let mut expected = vec![ali_id, ayse_id, hakan_id];
    expected.sort();
    let seen = roster(&app, &teacher, &board).await;
    assert_eq!(
        seen, expected,
        "an invite adds; it must never drop the hand-invited participant"
    );
    assert!(
        !seen.contains(&teacher_id),
        "the creator is a participant by construction and must not take a seat in the array: {seen:?}"
    );
}

/// An event source resolves whatever `GET /events/{id}/roster` resolves, and the
/// role filter runs over the result: a `parent`-role event names only parents,
/// and the whiteboard admits none of them. The call still succeeds — a source is
/// a whole group, and dropping its ineligible members must not fail it.
#[tokio::test]
async fn parents_in_a_source_are_dropped_silently_and_the_invite_still_succeeds() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher_c", "teacher").await;
    let _veli = login_as(&app, &db, "veli", "parent").await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&teacher),
        Some(json!({ "title": "Veli toplantısı", "audience": {"kind": "role", "role": "parent"} })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let event = res.body["id"].as_str().unwrap().to_string();

    let board = a_board(&app, &teacher).await;
    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "event", "event": event}),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "an all-parent source must be a successful no-op, not an error: {}",
        res.body
    );
    assert!(
        roster(&app, &teacher, &board).await.is_empty(),
        "a parent reached a whiteboard roster through bulk invite"
    );
}

/// A member the source still names but whose user row is gone is dropped the
/// same silent way — the batch lookup simply does not return them.
#[tokio::test]
async fn a_deleted_user_in_a_source_is_dropped() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher_d", "teacher").await;
    let ali = login(&app, "ali").await;
    let ghost = login(&app, "ghost").await;
    let (ali_id, ghost_id) = (me_id(&app, &ali).await, me_id(&app, &ghost).await);

    let course = create_course(&app, &teacher, "Fizik").await;
    enroll(&app, &teacher, &course, &ali_id).await;
    enroll(&app, &teacher, &course, &ghost_id).await;

    // The row goes without the cascade a real DELETE runs, which is exactly the
    // state a stale enrollment leaves behind.
    db.query("DELETE type::record('user', $key)")
        .bind(("key", ghost_id.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let board = a_board(&app, &teacher).await;
    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "course", "course": course}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        roster(&app, &teacher, &board).await,
        vec![ali_id],
        "a deleted user must not be written onto a roster"
    );
}

/// The cap is all-or-nothing. A partial invite would silently pick which half of
/// a class gets to draw, so an invite that would carry the board past
/// `max_participants` is refused whole and leaves the roster untouched.
#[tokio::test]
async fn an_over_cap_invite_is_refused_whole_and_changes_nothing() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager_e", "manager").await;
    let teacher = login_as(&app, &db, "teacher_e", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let class = a_class(&app, &manager, "9-B", &[&ali_id]).await;
    let board = a_board(&app, &teacher).await;

    // Fill the roster to the brim directly: reaching the cap through the API
    // would mean registering two hundred accounts to prove an arithmetic guard.
    // The ids need not resolve — the roster is only repaired at boot, and the
    // guard under test counts entries.
    let full: Vec<_> = (0..MAX_BOARD_PARTICIPANTS)
        .map(|n| UserId::from_key(&format!("filler{n}")).record())
        .collect();
    db.query("UPDATE $b SET participants = $who")
        .bind(("b", BoardId::from_key(&board).record()))
        .bind(("who", full))
        .await
        .unwrap()
        .check()
        .unwrap();
    let before = roster(&app, &teacher, &board).await;
    assert_eq!(before.len(), MAX_BOARD_PARTICIPANTS);

    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "class", "class": class}),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "one member over the cap must be a 409: {}",
        res.body
    );
    assert_eq!(
        roster(&app, &teacher, &board).await,
        before,
        "a refused invite must leave the roster byte-identical"
    );
}

/// The disclosure gate. A board's roster is visible to every participant, so
/// inviting a group hands that group's membership to everyone on the board —
/// which makes bulk invite a *read* of the source roster, and it must carry the
/// same gate that roster's own listing route carries. A student can still build
/// a board one id at a time; they cannot pour a class into one.
#[tokio::test]
async fn a_student_cannot_use_bulk_invite_as_a_class_roster_oracle() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager_f", "manager").await;
    let ali = login(&app, "ali").await;
    let ayse = login(&app, "ayse").await;
    let (ali_id, ayse_id) = (me_id(&app, &ali).await, me_id(&app, &ayse).await);

    let class = a_class(&app, &manager, "9-C", &[&ali_id, &ayse_id]).await;
    // Ali is *in* the class, and still may not dump it onto his own board.
    let board = a_board(&app, &ali).await;

    let res = invite(&app, &ali, &board, json!({"kind": "class", "class": class})).await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "a student expanded a class roster: {}",
        res.body
    );
    assert!(roster(&app, &ali, &board).await.is_empty(), "{}", res.body);
}

/// The course arm's gate is narrower than teacher+: a teacher who neither
/// created the course nor was assigned to it cannot read its roster, so they
/// cannot invite it either. The course's own teacher can.
#[tokio::test]
async fn a_teacher_who_does_not_run_the_course_cannot_invite_its_roster() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "teacher_g", "teacher").await;
    let outsider = login_as(&app, &db, "teacher_h", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let course = create_course(&app, &owner, "Kimya").await;
    enroll(&app, &owner, &course, &ali_id).await;

    let their_board = a_board(&app, &outsider).await;
    let res = invite(
        &app,
        &outsider,
        &their_board,
        json!({"kind": "course", "course": course}),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "an unrelated teacher read a course roster through an invite: {}",
        res.body
    );

    let own_board = a_board(&app, &owner).await;
    let res = invite(
        &app,
        &owner,
        &own_board,
        json!({"kind": "course", "course": course}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(roster(&app, &owner, &own_board).await, vec![ali_id]);
}

/// The board's own two-tier answer, unchanged by the new route: an outsider is
/// told the board does not exist, a participant who is not the creator is told
/// it is not theirs to command.
#[tokio::test]
async fn a_participant_gets_403_and_an_outsider_gets_404() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager_i", "manager").await;
    let creator = login_as(&app, &db, "teacher_i", "teacher").await;
    let other = login_as(&app, &db, "teacher_j", "teacher").await;
    let stranger = login_as(&app, &db, "teacher_k", "teacher").await;
    let other_id = me_id(&app, &other).await;

    let class = a_class(&app, &manager, "9-D", &[]).await;
    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&creator),
        Some(json!({ "title": "Tahta", "participants": [other_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let board = res.body["id"].as_str().unwrap().to_string();

    let res = invite(
        &app,
        &other,
        &board,
        json!({"kind": "class", "class": class.clone()}),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "a participant who is not the creator: {}",
        res.body
    );

    let res = invite(
        &app,
        &stranger,
        &board,
        json!({"kind": "class", "class": class}),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "an outsider must not learn the board exists: {}",
        res.body
    );
}

/// A source that does not exist is a `400` naming the field it came in on —
/// never a `404`, which on these routes means "no such board, or not yours" and
/// would tell a creator their own board had vanished.
#[tokio::test]
async fn an_unknown_source_is_a_400_and_not_the_boards_404() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher_l", "teacher").await;
    let board = a_board(&app, &teacher).await;

    for (source, field) in [
        (
            json!({"kind": "class", "class": "01J8XZ0K3Q8G7X2M4N5P6R7S8T"}),
            "class",
        ),
        (
            json!({"kind": "course", "course": "01J8XZ0K3Q8G7X2M4N5P6R7S8T"}),
            "course",
        ),
        (
            json!({"kind": "event", "event": "01J8XZ0K3Q8G7X2M4N5P6R7S8T"}),
            "event",
        ),
    ] {
        let res = invite(&app, &teacher, &board, source).await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "a missing {field} must not borrow the board's 404: {}",
            res.body
        );
        assert!(
            res.body.to_string().contains(field),
            "the 400 should name the field that was wrong: {}",
            res.body
        );
    }
}

/// The reply is the board itself, so a client never has to re-fetch to learn who
/// it just added. (That the live room is *told* as well is an end-to-end matter
/// and is pinned over a real socket in `e2e.rs`.)
#[tokio::test]
async fn the_reply_carries_the_widened_roster() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager_m", "manager").await;
    let teacher = login_as(&app, &db, "teacher_m", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let class = a_class(&app, &manager, "9-E", &[&ali_id]).await;
    let board = a_board(&app, &teacher).await;
    let res = invite(
        &app,
        &teacher,
        &board,
        json!({"kind": "class", "class": class}),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The reply is the board, so the roster the room is told about and the
    // roster the caller is shown are the same list.
    assert_eq!(
        res.body["participants"],
        json!([ali_id]),
        "the invite's own reply must carry the widened roster: {}",
        res.body
    );
}
