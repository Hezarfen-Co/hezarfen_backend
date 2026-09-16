//! Regressions for the counter/cap defects found in the 2026-08-02 sweep:
//! settings retirement rollback, the exam-kind gate on grading, snapshotted
//! caps, and the whiteboard clear marker.
//!
//! Everything here is deterministic. Where a defect only bites a true race, the
//! *reachable half* is driven instead — the corrupt state the race leaves
//! behind is written straight into the store, and the assertion is on what the
//! store holds afterwards. The in-memory engine forges wins under concurrency
//! (src/domain/cap.rs), so no test below asks it who won.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, create_exam, enroll, id_of, login, login_as, me_id, send, taught_under};
use hezarfen_backend::domain::board::BoardId;
use hezarfen_backend::domain::user::UserId;
use serde_json::json;

/// The default kinds, minus one — the body a manager sends to drop `yazili`.
fn kinds_without_yazili() -> serde_json::Value {
    json!({"exam_kinds": [{"name": "sozlu", "weight": 1}, {"name": "uygulama", "weight": 1}]})
}

/// Grading is gated twice: by the school's list and by the kind's reference
/// counter. Only the counter used to be consulted, so *any* disagreement
/// between the two records reopened grading under a kind nobody offers — which
/// is exactly what a settings PATCH whose save lost the row's compare-and-set
/// used to leave behind (it un-retired the winner's kind). The list is the
/// school's own answer, so it refuses on its own.
#[tokio::test]
async fn a_kind_off_the_schools_list_cannot_be_graded_even_with_a_live_counter() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let student = login(&app, "ogrenci").await;
    let student_id = me_id(&app, &student).await;
    let t = taught_under(&app, &manager, &teacher, "Matematik").await;
    enroll(&app, &teacher, &t.instance, &student_id).await;
    let exam = create_exam(&app, &teacher, &t.instance, &t.term, "Vize", "yazili").await;

    // The kind leaves the list — legal, nothing is graded under it yet.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(kinds_without_yazili()),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The corrupt half a lost settings race used to leave: the name is off the
    // list, but its counter says "in service".
    sqlx::query(
        "INSERT INTO kind_ref (name, count, retired) VALUES ('yazili', 0, false)
         ON CONFLICT (name) DO UPDATE SET retired = false",
    )
    .execute(&db)
    .await
    .unwrap();

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "user_id": student_id, "mark": 80 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"]
            .as_str()
            .unwrap()
            .contains("removed from the school's settings"),
        "{}",
        res.body
    );

    // And nothing landed: no mark, and the exam's result counter never moved.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0);
}

/// Putting the kind back in the list makes the very same grade land — the gate
/// is the list, not a one-way door.
#[tokio::test]
async fn re_adding_the_kind_lets_the_grade_land() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let student = login(&app, "ogrenci").await;
    let student_id = me_id(&app, &student).await;
    let t = taught_under(&app, &manager, &teacher, "Matematik").await;
    enroll(&app, &teacher, &t.instance, &student_id).await;
    let exam = create_exam(&app, &teacher, &t.instance, &t.term, "Vize", "yazili").await;
    let grade = json!({ "user_id": student_id, "mark": 80 });

    send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(kinds_without_yazili()),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(grade.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({"exam_kinds": [
            {"name": "yazili", "weight": 2},
            {"name": "sozlu", "weight": 1},
            {"name": "uygulama", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(grade),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// The chatbot cap lives on the *settings singleton*, not on the row the seat
/// is taken on, so the claim sub-queries it inside its own conditional write.
/// A cap read at the wrong place answers one of two ways — always the built-in
/// default (the school's edit ignored) or always zero (nobody may ever start a
/// thread) — and this pins it against both.
#[tokio::test]
async fn the_chatbot_cap_is_the_one_on_the_settings_row() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let user = login(&app, "ali").await;

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_threads": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // One thread fits the school's cap of 1...
    let res = start_thread(&app, &user).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // ...the second does not. (Under the built-in default of 50 it would.)
    let res = start_thread(&app, &user).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Raising the cap re-opens it, which no always-zero read could do.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_threads": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = start_thread(&app, &user).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// Open one chatbot thread as `cookie`.
async fn start_thread(app: &axum::Router, cookie: &str) -> common::Res {
    send(
        app,
        "POST",
        "/chatbot/threads",
        Some(cookie),
        Some(json!({ "title": "Soru" })),
    )
    .await
}

/// The registration cap is read off `audience.capacity` on the event row as the
/// seat is taken, not bound as a number the handler read first. A mis-aimed
/// live read is an *unlimited* list (the column reads `NONE`), so the refusal
/// below is what proves the expression lands on the real column.
#[tokio::test]
async fn a_registration_list_is_capped_by_the_column_on_its_own_row() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let first = login(&app, "ali").await;
    let second = login(&app, "veli").await;
    let first_id = me_id(&app, &first).await;
    let second_id = me_id(&app, &second).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&teacher),
        Some(json!({
            "title": "Gezi",
            "audience": { "kind": "registration", "capacity": 1 },
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let event = id_of(&res.body);

    let res = seat(&app, &teacher, &event, &first_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = seat(&app, &teacher, &event, &second_id).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // The refusal wrote nothing: one seat, one row.
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

/// A whiteboard clear mints a real `board_stroke` row (the epoch marker) and
/// claims neither counter, so a clear of an already-blank canvas was a free row
/// — pressed in a loop, an unbounded table behind a cap that only ever counted
/// strokes *drawn*. Nothing to clear is now a refusal, which pays for every
/// marker with at least one stroke it closes.
#[tokio::test]
async fn a_blank_canvas_cannot_be_cleared_for_a_free_row() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Geometri", "participants": [] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let board = id_of(&res.body);
    // A board nobody has drawn on has nothing to clear.
    let res = clear(&app, &ali, &board).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(rows(&db, &board).await, 0, "the refusal minted no marker");

    // One mark pays for one marker...
    hezarfen_backend::db::board_stroke::append(
        &db,
        &BoardId::from_key(&board),
        &UserId::from_key(&ali_id),
        "{\"p\":[1,2]}",
        0,
    )
    .await
    .expect("append");
    let res = clear(&app, &ali, &board).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(rows(&db, &board).await, 2, "the stroke and its marker");

    // ...and the blank canvas it leaves buys no more.
    let res = clear(&app, &ali, &board).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(rows(&db, &board).await, 2);
    assert_eq!(
        stored_epoch(&db, &board).await,
        1,
        "a refused clear must not move the epoch either"
    );
}

/// End the board's current epoch as `cookie`.
async fn clear(app: &axum::Router, cookie: &str, board: &str) -> common::Res {
    send(
        app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(cookie),
        None,
    )
    .await
}

/// Rows on `board_stroke` for one board, markers included.
async fn rows(db: &hezarfen_backend::database::Database, board: &str) -> usize {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM board_stroke WHERE board = $1")
        .bind(BoardId::from_key(board))
        .fetch_one(db)
        .await
        .unwrap() as usize
}

/// The board's stored epoch — the clear's other half.
async fn stored_epoch(db: &hezarfen_backend::database::Database, board: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT epoch FROM board WHERE id = $1")
        .bind(BoardId::from_key(board))
        .fetch_one(db)
        .await
        .unwrap()
}

/// The clear marker is a row on the table, so the lifetime counter has to
/// count it. Charging only the strokes made `total_stroke_count` under-count
/// the rows it is supposed to bound by one per clear — `MAX_BOARD_STROKES`
/// stopped meaning "rows on this board". The counter is asserted against the
/// rows the store actually holds, which is the invariant, not a number.
#[tokio::test]
async fn a_clear_marker_is_charged_to_the_lifetime_counter() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Geometri", "participants": [] })),
    )
    .await;
    let board = id_of(&res.body);
    for _ in 0..2 {
        hezarfen_backend::db::board_stroke::append(
            &db,
            &BoardId::from_key(&board),
            &UserId::from_key(&ali_id),
            "{\"p\":[1,2]}",
            0,
        )
        .await
        .expect("append");
    }
    let res = clear(&app, &ali, &board).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // Two strokes plus the marker that closed them: three rows, and the
    // lifetime counter says three. The epoch counter reset, as always.
    assert_eq!(rows(&db, &board).await, 3);
    assert_eq!(counters(&db, &board).await, (0, 3));

    // The next epoch keeps counting from there — a marker is never re-counted
    // and never uncounted.
    hezarfen_backend::db::board_stroke::append(
        &db,
        &BoardId::from_key(&board),
        &UserId::from_key(&ali_id),
        "{\"p\":[3,4]}",
        1,
    )
    .await
    .expect("append");
    clear(&app, &ali, &board).await;
    assert_eq!(rows(&db, &board).await, 5);
    assert_eq!(counters(&db, &board).await, (0, 5));
}

/// `(epoch_stroke_count, total_stroke_count)` as the store holds them.
async fn counters(db: &hezarfen_backend::database::Database, board: &str) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT epoch_stroke_count, total_stroke_count FROM board WHERE id = $1",
    )
    .bind(BoardId::from_key(board))
    .fetch_one(db)
    .await
    .unwrap()
}

/// A menu's record id is `<date>_<slot>` and that id is a URL path segment, so
/// a slot named `a/b` is a slot no menu can ever be published under. The menu
/// side refuses those characters; the settings side used to accept them, which
/// let a school define a slot the canteen could never use — a dead entry only
/// another settings PATCH could clear.
#[tokio::test]
async fn a_slot_name_that_no_menu_id_could_carry_is_refused_by_settings() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    for bad in ["a/b", "a\\b", "a?b", "a#b", "a%b"] {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(&manager),
            Some(json!({"meal_slots": [{"name": bad}]})),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "slot {bad} was accepted: {}",
            res.body
        );
    }
    // The legal name still lands, and the refusals left the list alone.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({"meal_slots": [{"name": "öğle yemeği"}]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["meal_slots"][0]["name"], "öğle yemeği");

    // Stale data: a row written before this rule keeps its name, and must not
    // wedge an edit that never touches the meal slots. The carried-over list
    // goes back into the row as it stands (`MealSlotDef::try_new` runs on the
    // *request's* slots only), so an unrelated PATCH still lands.
    sqlx::query("UPDATE settings SET meal_slots = $1 WHERE id = 'school'")
        .bind(json!([{ "name": "a/b", "serving_minute": null }]))
        .execute(&db)
        .await
        .unwrap();
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({"max_file_bytes": 1048576})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["max_file_bytes"], 1048576);
    assert_eq!(
        res.body["meal_slots"][0]["name"], "a/b",
        "the stale slot rides through untouched"
    );
}


/// A slot name the school *already stores* must stay submittable.
///
/// The URL-safety rule on slot names is younger than the lists it validates,
/// and `PATCH /settings` re-validates the whole submitted list — so a school
/// holding a pre-rule name was locked out of `meal_slots` entirely: re-sending
/// the name 400s, and dropping it 409s the moment a menu references it. The
/// stored name is grandfathered; a new one with the same characters is not.
#[tokio::test]
async fn a_stale_slot_name_no_longer_wedges_the_rest_of_the_list() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "slot_wedge_admin", "admin").await;

    // A legal edit first: `UPDATE` on the singleton is a no-op until the row
    // exists, and an absent row reads as the built-in defaults.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [{"name": "lunch"}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The list as a database written before the rule holds it, plus the menu
    // reference that makes dropping the name a 409.
    sqlx::query("UPDATE settings SET meal_slots = $1 WHERE id = 'school'")
        .bind(json!([
            { "name": "a/b", "serving_minute": null },
            { "name": "lunch", "serving_minute": null },
        ]))
        .execute(&db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO slot_ref (name, count, retired) VALUES ('a/b', 1, false)
         ON CONFLICT (name) DO UPDATE SET count = 1",
    )
    .execute(&db)
    .await
    .unwrap();

    // Editing the *rest* of the list works, stale name carried along.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [
            {"name": "a/b"}, {"name": "lunch"}, {"name": "kahvaltı"},
        ] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Dropping it is still the 409 the published menu earns it...
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [{"name": "lunch"}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // ...and a name nobody stored is still refused outright.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [{"name": "a/b"}, {"name": "lunch"}, {"name": "c/d"}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}
