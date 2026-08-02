//! Regressions for the role sweep and the parent-delete cascades found in the
//! 2026-08-02 sweep: an event seat a demotion left claimed forever, and the
//! two-query deletes that could orphan a child row.
//!
//! Every assertion re-reads the *store*. The in-memory engine forges wins under
//! concurrency (src/domain/cap.rs), so nothing below is judged on a response
//! body or asked who won a race — the cascade tests drive the reachable half
//! and pin what the store holds once the transaction is done.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, id_of, login, login_as, me_id, send};
use hezarfen_backend::database::Database;
use serde_json::json;

/// One counter, re-read out of the store — never off a response body.
async fn counter(sql: &str, db: &Database) -> i64 {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result
        .take::<Vec<i64>>(0)
        .unwrap()
        .first()
        .copied()
        .unwrap_or(0)
}

/// How many rows `sql` selects ids for.
async fn rows(sql: &str, db: &Database) -> i64 {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len() as i64
}

/// Put `user` on `event`'s signup list as `cookie`.
async fn seat(app: &axum::Router, cookie: &str, event: &str, user: &str) -> common::Res {
    send(
        app,
        "POST",
        &format!("/events/{event}/register"),
        Some(cookie),
        Some(json!({ "user_id": user })),
    )
    .await
}

/// A one-seat signup list.
async fn capped_event(app: &axum::Router, teacher: &str, capacity: Option<i64>) -> String {
    let audience = match capacity {
        Some(capacity) => json!({ "kind": "registration", "capacity": capacity }),
        None => json!({ "kind": "registration" }),
    };
    let res = send(
        app,
        "POST",
        "/events",
        Some(teacher),
        Some(json!({ "title": "Gezi", "audience": audience })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

/// Free `user`'s seat on `event` as `cookie`.
async fn free_seat(app: &axum::Router, cookie: &str, event: &str, user: &str) -> common::Res {
    send(
        app,
        "DELETE",
        &format!("/events/{event}/register/{user}"),
        Some(cookie),
        None,
    )
    .await
}

/// Push an event's start into the past, which is how its signup list freezes.
/// The API refuses to *schedule* one there (60s grace), so the row is aged in
/// the store — the state a real event reaches by the clock simply running on.
async fn age_event(event: &str, db: &Database) {
    db.query("UPDATE type::record('event', $key) SET starts_at = 1")
        .bind(("key", event.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// The role sweep dropped enrollments, class memberships, parent links and
/// course staffing — but not signups. A `parent`'s seat is the one that is
/// genuinely *unfreeable*: they cannot reach `DELETE /events/{id}/register/…`
/// (teacher+ only) and no one else may free a non-student's seat, so a
/// capacity-1 event answered "full" forever and the stale holder stayed on the
/// roster (a registration audience resolves on the row's mere existence).
#[tokio::test]
async fn a_demotion_frees_the_event_seat_nothing_else_could_free() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;

    let event = capped_event(&app, &teacher, Some(1)).await;
    let res = seat(&app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        1
    );

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The row goes and the seat comes back with it — one is worthless without
    // the other.
    assert_eq!(
        rows("SELECT VALUE id FROM registration", &db).await,
        0,
        "a non-student may hold no signup row"
    );
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        0,
        "the seat must be handed back, or the event is full forever"
    );

    // And the freed seat is really usable.
    let res = seat(&app, &teacher, &event, &veli_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/events/{event}/roster"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
}

/// The mirror ruling: a *promotion* strands nothing, because staff free their
/// own seats by hand. Sweeping there destroyed a signup a teacher is entitled
/// to keep — and irreversibly, since re-demoting restores nothing. The seat
/// stays, and the promoted teacher can still give it back themselves.
#[tokio::test]
async fn a_promotion_keeps_the_signup_the_promoted_user_can_still_free() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let event = capped_event(&app, &teacher, Some(1)).await;
    assert_eq!(
        seat(&app, &teacher, &event, &ali_id).await.status,
        StatusCode::OK
    );

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        rows("SELECT VALUE id FROM registration", &db).await,
        1,
        "a promotion must not destroy a signup its holder can still free"
    );
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        1,
        "…and the seat it holds stays claimed with it"
    );

    // Which is the whole justification: the seat is still theirs to free.
    let promoted = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({
            "username": "ali", "password": "secret1"
        })),
    )
    .await
    .cookie
    .expect("re-login");
    let res = free_seat(&app, &promoted, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert_eq!(rows("SELECT VALUE id FROM registration", &db).await, 0);
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        0
    );
}

/// A signup list freezes when its event starts, and from then on it is
/// historical record: `register` and `unregister` both answer 409. The sweep
/// obeys the same freeze — rewriting a closed roster behind its back is
/// irrecoverable, since re-registering is refused too.
#[tokio::test]
async fn a_demotion_leaves_a_frozen_signup_list_exactly_as_it_stands() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let event = capped_event(&app, &teacher, Some(1)).await;
    assert_eq!(
        seat(&app, &teacher, &event, &ali_id).await.status,
        StatusCode::OK
    );
    age_event(&event, &db).await;

    // The freeze bites for the route…
    let res = free_seat(&app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // …so it must bite for the sweep as well.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        rows("SELECT VALUE id FROM registration", &db).await,
        1,
        "a closed roster must not be rewritten by a role change"
    );
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        1,
        "…and its seat stays claimed with it — the row is the seat"
    );
}

/// An orphan signup — its event record gone — has no seat to give back and no
/// list that can freeze, and `unregister` answers 404 on the missing event. So
/// the sweep must *delete* it rather than skip it: skipped, it is the exact
/// unremovable row the sweep exists to prevent. (Low reach — `Event::delete`
/// cascades in one transaction — but this is the class, not a coincidence.)
#[tokio::test]
async fn the_sweep_deletes_an_orphan_signup_instead_of_stranding_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let live = capped_event(&app, &teacher, None).await;
    let doomed = capped_event(&app, &teacher, None).await;
    assert_eq!(
        seat(&app, &teacher, &live, &ali_id).await.status,
        StatusCode::OK
    );
    assert_eq!(
        seat(&app, &teacher, &doomed, &ali_id).await.status,
        StatusCode::OK
    );

    // The state a delete that raced a register used to leave: the row outlives
    // its event. Written straight into the store, since the cascade now makes
    // it unreachable through the API.
    db.query("DELETE type::record('event', $key)")
        .bind(("key", doomed.clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        rows("SELECT VALUE id FROM registration", &db).await,
        0,
        "an orphan signup must go with the rest — nothing else can ever remove it"
    );
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        0,
        "and the surviving event still gets its seat back"
    );
}

/// How many boards list `user` as a participant, read out of the store.
async fn boards_listing(user: &str, db: &Database) -> i64 {
    let mut result = db
        .query("SELECT VALUE id FROM board WHERE type::record('user', $u) IN participants")
        .bind(("u", user.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len() as i64
}

/// A `parent` is barred from the whiteboard outright, and invites already
/// refuse the role — but `set_role` never touched a roster, so a demotion left
/// the user listed forever: still fanned every stroke, and a stale id that 400s
/// the creator's next roster PATCH. The sweep drops them from every list and
/// **deletes nothing** — not a board, not a stroke, not even the board they
/// created, whose room carries on for everyone else with a creator who can no
/// longer use it (a role change must not destroy other people's work; there is
/// no way to hand a board over, so closing or deleting it would be the role
/// change silently ending a live session).
#[tokio::test]
async fn a_demotion_to_parent_leaves_every_board_roster_and_deletes_nothing() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;

    // One board veli was invited to, one board veli created.
    let guest = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Geometri", "participants": [veli_id] })),
    )
    .await;
    assert_eq!(guest.status, StatusCode::CREATED, "{}", guest.body);
    let own = send(
        &app,
        "POST",
        "/boards",
        Some(&veli),
        Some(json!({ "title": "Cebir", "participants": [ali_id] })),
    )
    .await;
    assert_eq!(own.status, StatusCode::CREATED, "{}", own.body);
    let own_id = id_of(&own.body);
    assert_eq!(boards_listing(&veli_id, &db).await, 1);

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{veli_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        boards_listing(&veli_id, &db).await,
        0,
        "a parent may appear on no board roster"
    );
    // Nothing was destroyed, and the board they created is untouched beyond
    // that: same creator, and its other participant still on it.
    assert_eq!(rows("SELECT VALUE id FROM board", &db).await, 2);
    let res = send(&app, "GET", &format!("/boards/{own_id}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["creator"], veli_id);
    assert_eq!(res.body["participants"], json!([ali_id]));

    // And the demoted creator is barred from their own board, by role alone.
    let res = send(&app, "GET", &format!("/boards/{own_id}"), Some(&veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// The cascade and the parent row now commit together. Run as two queries, a
/// register or a mark landing in between outlived its event — an orphan keyed
/// on a row that no longer exists, so no read path could reach it and no delete
/// could reclaim it. The state is asserted against the store, not the 204.
#[tokio::test]
async fn deleting_an_event_takes_its_children_with_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let event = capped_event(&app, &teacher, None).await;
    let res = seat(&app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&teacher),
        Some(json!({ "user_id": ali_id, "status": "present" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(rows("SELECT VALUE id FROM attendance", &db).await, 1);

    let res = send(
        &app,
        "DELETE",
        &format!("/events/{event}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    assert_eq!(rows("SELECT VALUE id FROM event", &db).await, 0);
    assert_eq!(
        rows("SELECT VALUE id FROM attendance", &db).await,
        0,
        "no mark may outlive its event"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM registration", &db).await,
        0,
        "no signup may outlive its event"
    );
}

/// The note half of the same defect: the attachment rows and the note commit
/// together, so an upload can no longer leave a `note_file` pointing at a note
/// that is gone (its *blob* is a known, documented leak — see `Note::delete`).
#[tokio::test]
async fn deleting_a_note_takes_its_attachment_rows_with_it() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "Plan", "content": "yarın" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let note = id_of(&res.body);

    let up = common::upload_file(&app, &ali, &note, "plan.txt", "text/plain", b"merhaba").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(rows("SELECT VALUE id FROM note_file", &db).await, 1);

    let res = send(&app, "DELETE", &format!("/notes/{note}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    assert_eq!(rows("SELECT VALUE id FROM note", &db).await, 0);
    assert_eq!(
        rows("SELECT VALUE id FROM note_file", &db).await,
        0,
        "no attachment row may outlive its note"
    );
}
