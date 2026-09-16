//! Pagination behavior common to every list endpoint: the `{items, total,
//! limit, offset}` envelope, opt-in windowing, join-on-the-page, and the `400`s
//! for out-of-range parameters. One representative endpoint per shape — the
//! machinery (`web::page`) is shared, so this pins the contract without
//! re-testing all 16 lists.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, create_course, enroll, items, login, login_as, me_id, send, total};
use serde_json::json;

/// Register `user00..user{n-1}` as plain students.
async fn seed_students(app: &axum::Router, n: usize) {
    for i in 0..n {
        let name = format!("user{i:02}");
        let creds = json!({ "school": "demo", "username": name, "password": "secret1" });
        let res = send(app, "POST", "/auth/register", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::CREATED, "register {name}");
    }
}

#[tokio::test]
async fn envelope_windows_and_total_is_stable() {
    let (app, db) = app_and_db().await;
    // 1 admin (to read /users) + 24 students = 25 accounts.
    let admin = login_as(&app, &db, "boss", "admin").await;
    seed_students(&app, 24).await;

    // Unpaged: the whole list, `limit` echoes null, `total` counts everyone.
    let res = send(&app, "GET", "/users", Some(&admin), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(total(&res.body), 25);
    assert_eq!(items(&res.body).len(), 25);
    assert!(res.body["limit"].is_null(), "omitted limit echoes null");
    assert_eq!(res.body["offset"], 0);

    // First window of 10.
    let res = send(&app, "GET", "/users?limit=10&offset=0", Some(&admin), None).await;
    assert_eq!(total(&res.body), 25);
    assert_eq!(items(&res.body).len(), 10);
    assert_eq!(res.body["limit"], 10);
    assert_eq!(res.body["offset"], 0);
    let page1: Vec<_> = items(&res.body).iter().map(|u| u["id"].clone()).collect();

    // Second window — disjoint from the first, identical total.
    let res = send(&app, "GET", "/users?limit=10&offset=10", Some(&admin), None).await;
    assert_eq!(total(&res.body), 25);
    assert_eq!(items(&res.body).len(), 10);
    assert_eq!(res.body["offset"], 10);
    let page2: Vec<_> = items(&res.body).iter().map(|u| u["id"].clone()).collect();
    assert!(
        page2.iter().all(|id| !page1.contains(id)),
        "consecutive pages must not overlap"
    );

    // Tail — the remaining 5, even though a full window of 10 was asked for.
    let res = send(&app, "GET", "/users?limit=10&offset=20", Some(&admin), None).await;
    assert_eq!(items(&res.body).len(), 5, "tail returns only the remainder");
    assert_eq!(total(&res.body), 25);

    // Past the end — an empty page, not an error, and `total` stays honest.
    let res = send(
        &app,
        "GET",
        "/users?limit=10&offset=999",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(items(&res.body).is_empty(), "offset past the end is empty");
    assert_eq!(total(&res.body), 25);
}

#[tokio::test]
async fn bad_paging_params_are_rejected() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;

    for bad in [
        "limit=0",   // below the floor
        "limit=501", // above the 500 cap
        "limit=-1",  // negative
        "offset=-1", // negative offset
        "limit=abc", // not an integer
    ] {
        let res = send(&app, "GET", &format!("/users?{bad}"), Some(&admin), None).await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "GET /users?{bad} should be 400"
        );
    }

    // The inclusive bounds and a bare offset are accepted.
    for ok in ["limit=1", "limit=500", "offset=0", "limit=50&offset=0"] {
        let res = send(&app, "GET", &format!("/users?{ok}"), Some(&admin), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET /users?{ok} should be 200");
    }
}

#[tokio::test]
async fn paged_roster_still_embeds_people_on_the_page() {
    // A list that joins person refs must run that join over the returned page,
    // not the whole table — the page rows still carry their embedded people.
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let course = create_course(&app, &teacher, "Algebra").await;

    for i in 0..5 {
        let student = login(&app, &format!("stud{i}")).await;
        let sid = me_id(&app, &student).await;
        enroll(&app, &teacher, &course, &sid).await;
    }

    let uri = format!("/courses/{course}/enrollments?limit=2&offset=0");
    let res = send(&app, "GET", &uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(total(&res.body), 5, "total counts the whole roster");
    let page = items(&res.body);
    assert_eq!(page.len(), 2, "only the requested window comes back");
    for row in page {
        assert!(
            row["user"]["username"].is_string(),
            "enrolled student ref present on the page: {row}"
        );
        assert!(
            row["enrolled_by"]["username"].is_string(),
            "enroller ref present on the page: {row}"
        );
    }
}

/// Far enough ahead to clear the not-in-the-past guard without touching the
/// clock, so the appointment lists below never race the wall clock.
const SLOT_START: i64 = 1_900_000_000_000;
const WEEK: i64 = 7 * 24 * 60 * 60 * 1000;

/// Publish `n` weekly occurrences as `teacher`; returns their ids, earliest
/// first — the order both appointment lists page in.
async fn publish_slots(app: &axum::Router, teacher: &str, n: i64) -> Vec<String> {
    let res = send(
        app,
        "POST",
        "/appointments/slots",
        Some(teacher),
        Some(json!({
            "starts_at": SLOT_START,
            "ends_at": SLOT_START + 1_800_000,
            "repeat_weekly": true,
            "until": SLOT_START + (n - 1) * WEEK,
        })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "publish slots: {}",
        res.body
    );
    let slots = res
        .body
        .as_array()
        .expect("publish returns an array")
        .clone();
    assert_eq!(slots.len() as i64, n, "one row per week");
    slots
        .iter()
        .map(|slot| slot["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn appointment_slots_are_paged() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ali", "teacher").await;
    publish_slots(&app, &teacher, 6).await;

    // Unpaged: the whole calendar, `limit` echoes null, `total` counts it all.
    let res = send(&app, "GET", "/appointments/slots", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(total(&res.body), 6);
    assert_eq!(items(&res.body).len(), 6, "omitting limit returns them all");
    assert!(res.body["limit"].is_null());
    assert_eq!(res.body["offset"], 0);

    // A window: `total` still carries the unpaged count.
    let res = send(
        &app,
        "GET",
        "/appointments/slots?limit=2&offset=4",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(total(&res.body), 6, "total is the unpaged count");
    assert_eq!(items(&res.body).len(), 2);
    assert_eq!(res.body["limit"], 2);
    assert_eq!(res.body["offset"], 4);

    // Past the end — empty page, honest total, and the person join still rides
    // along on the rows that do come back.
    let res = send(
        &app,
        "GET",
        "/appointments/slots?limit=2&offset=99",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(items(&res.body).is_empty());
    assert_eq!(total(&res.body), 6);

    // A student browses the same calendar — bookable slots, paged identically.
    let student = login(&app, "ayse").await;
    let res = send(
        &app,
        "GET",
        "/appointments/slots?limit=3&offset=0",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(total(&res.body), 6);
    let page = items(&res.body);
    assert_eq!(page.len(), 3);
    for slot in page {
        assert_eq!(
            slot["teacher"]["username"], "ali",
            "teacher ref present on the page: {slot}"
        );
    }
}

#[tokio::test]
async fn appointments_are_paged() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ali", "teacher").await;
    let student = login(&app, "ayse").await;
    // Five weekly slots, all booked by the same student. Distinct weeks, so no
    // booking collides with another.
    let slots = publish_slots(&app, &teacher, 5).await;
    for slot in &slots {
        let res = send(
            &app,
            "POST",
            "/appointments",
            Some(&student),
            Some(json!({ "slot": slot, "reason": "ödev" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "book {slot}: {}", res.body);
    }

    // Unpaged: every booking the student asked for.
    let res = send(&app, "GET", "/appointments", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(total(&res.body), 5);
    assert_eq!(items(&res.body).len(), 5, "omitting limit returns them all");
    assert!(res.body["limit"].is_null());
    assert_eq!(res.body["offset"], 0);

    // Consecutive windows are disjoint and share one total.
    let res = send(
        &app,
        "GET",
        "/appointments?limit=2&offset=0",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(total(&res.body), 5, "total is the unpaged count");
    assert_eq!(res.body["limit"], 2);
    let page1: Vec<_> = items(&res.body).iter().map(|a| a["id"].clone()).collect();
    let res = send(
        &app,
        "GET",
        "/appointments?limit=2&offset=2",
        Some(&student),
        None,
    )
    .await;
    let page2: Vec<_> = items(&res.body).iter().map(|a| a["id"].clone()).collect();
    assert!(page2.iter().all(|id| !page1.contains(id)));

    // The tail is the remainder, and the slot/person join runs over the page.
    let res = send(
        &app,
        "GET",
        "/appointments?limit=2&offset=4",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(total(&res.body), 5);
    let page = items(&res.body);
    assert_eq!(page.len(), 1, "offset 4 of 5 leaves 1");
    assert_eq!(page[0]["requester"]["username"], "ayse");
    assert_eq!(page[0]["teacher"]["username"], "ali");

    // The teacher's inbox is the same five bookings, paged the same way.
    let res = send(
        &app,
        "GET",
        "/appointments?limit=3&offset=0",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(total(&res.body), 5, "the teacher's inbox counts all five");
    assert_eq!(items(&res.body).len(), 3);
}

#[tokio::test]
async fn user_search_is_paged_not_capped() {
    // `/users/search` used to hard-cap at 10 rows; it now pages like the rest.
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    // 12 accounts sharing the "grp" fragment — past the old cap.
    for i in 0..12 {
        let creds =
            json!({ "school": "demo", "username": format!("grp{i:02}"), "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    // Unpaged: every match comes back — no 10-row ceiling.
    let res = send(&app, "GET", "/users/search?q=grp", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        total(&res.body),
        12,
        "all matches counted, not capped at 10"
    );
    assert_eq!(items(&res.body).len(), 12);

    // Windowed like any other list — the tail is the remainder.
    let res = send(
        &app,
        "GET",
        "/users/search?q=grp&limit=5&offset=10",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(total(&res.body), 12);
    assert_eq!(items(&res.body).len(), 2, "offset 10 of 12 leaves 2");

    // Paging bounds are enforced.
    let res = send(
        &app,
        "GET",
        "/users/search?q=grp&limit=0",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "limit=0 is a 400");

    // A blank `q` is the opening directory, not an error: everyone the caller
    // may see — the 12 `grp` accounts plus the teacher themselves.
    let res = send(&app, "GET", "/users/search?q=", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(total(&res.body), 13, "blank q lists everyone visible");

    // Blank q scoped to a role is a listing, not an error.
    let res = send(
        &app,
        "GET",
        "/users/search?q=&role=student",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(total(&res.body), 12, "blank q + role lists the whole role");
    let res = send(
        &app,
        "GET",
        "/users/search?q=&role=teacher",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(total(&res.body), 1, "role scope excludes the students");
}

/// The whiteboard's four lists all speak the envelope: the board list, the live
/// canvas, the whole history and the epoch index. `/epochs` is the odd one —
/// it pages in the web layer over a bounded read (`web/boards.rs:391`), so it
/// gets the same windowing proof as the three DB-paged lists.
#[tokio::test]
async fn board_lists_are_paged() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    // Three boards, and one of them carries the strokes.
    let mut boards = Vec::new();
    for n in 0..3 {
        let res = send(
            &app,
            "POST",
            "/boards",
            Some(&ali),
            Some(json!({ "title": format!("Tahta {n}") })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        boards.push(res.body["id"].as_str().unwrap().to_string());
    }
    let board = boards[0].clone();

    // Two closed epochs of 2 marks each, then 5 live ones: 2 markers in the
    // index, 11 rows in the history (4 marks + 2 markers + 5 live), 5 on the
    // canvas.
    for epoch in 0..2 {
        for n in 0..2 {
            hezarfen_backend::db::board_stroke::append(
                &db,
                &hezarfen_backend::domain::board::BoardId::from_key(&board),
                &hezarfen_backend::domain::user::UserId::from_key(&ali_id),
                &format!("{{\"p\":[{epoch},{n}]}}"),
                epoch,
            )
            .await
            .expect("append");
        }
        let res = send(
            &app,
            "POST",
            &format!("/boards/{board}/clear"),
            Some(&ali),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    for n in 0..5 {
        hezarfen_backend::db::board_stroke::append(
            &db,
            &hezarfen_backend::domain::board::BoardId::from_key(&board),
            &hezarfen_backend::domain::user::UserId::from_key(&ali_id),
            &format!("{{\"p\":[2,{n}]}}"),
            2,
        )
        .await
        .expect("append");
    }

    for (uri, count) in [
        ("/boards".to_string(), 3),
        (format!("/boards/{board}/strokes"), 5),
        (format!("/boards/{board}/history"), 11),
        (format!("/boards/{board}/epochs"), 2),
    ] {
        // Unpaged: everything, `limit` echoes null.
        let res = send(&app, "GET", &uri, Some(&ali), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {uri}: {}", res.body);
        assert_eq!(total(&res.body), count, "GET {uri} total");
        assert_eq!(items(&res.body).len() as i64, count, "GET {uri} items");
        assert!(res.body["limit"].is_null(), "GET {uri} echoes a null limit");
        assert_eq!(res.body["offset"], 0);

        // A window of 1: the total stays the unpaged count, the windows are
        // disjoint, and the page past the end is empty rather than an error.
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=1&offset=0"),
            Some(&ali),
            None,
        )
        .await;
        assert_eq!(total(&res.body), count, "GET {uri} windowed total");
        assert_eq!(items(&res.body).len(), 1, "GET {uri} window size");
        assert_eq!(res.body["limit"], 1);
        let first = items(&res.body)[0]["id"].clone();
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=1&offset=1"),
            Some(&ali),
            None,
        )
        .await;
        assert_eq!(res.body["offset"], 1);
        assert_ne!(items(&res.body)[0]["id"], first, "GET {uri} pages overlap");
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=1&offset=99"),
            Some(&ali),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
        assert!(items(&res.body).is_empty(), "GET {uri} past the end");
        assert_eq!(total(&res.body), count, "GET {uri} total stays honest");

        // The shared bounds bite on every one of them.
        for bad in ["limit=0", "limit=501", "offset=-1"] {
            let res = send(&app, "GET", &format!("{uri}?{bad}"), Some(&ali), None).await;
            assert_eq!(
                res.status,
                StatusCode::BAD_REQUEST,
                "GET {uri}?{bad} should be 400"
            );
        }
    }

    // `?epoch=` narrows the history and still pages: epoch 0 is 2 marks plus
    // the marker that closed it.
    let uri = format!("/boards/{board}/history?epoch=0");
    let res = send(&app, "GET", &uri, Some(&ali), None).await;
    assert_eq!(total(&res.body), 3, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("{uri}&limit=2&offset=2"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(total(&res.body), 3, "the scoped total is the scoped count");
    assert_eq!(items(&res.body).len(), 1, "offset 2 of 3 leaves 1");

    // `?open=` narrows the board list the same way `?read=` narrows a message
    // folder: the filter reaches the database, so `total` is the filtered count
    // and the window still pages. A closed board is never deleted, so this is
    // the only way a heavy creator trims the retired ones out.
    let res = send(
        &app,
        "POST",
        &format!("/boards/{}/close", boards[2]),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for (query, count) in [("", 3), ("?open=true", 2), ("?open=false", 1)] {
        let res = send(&app, "GET", &format!("/boards{query}"), Some(&ali), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "GET /boards{query}: {}",
            res.body
        );
        assert_eq!(total(&res.body), count, "GET /boards{query} total");
        assert_eq!(items(&res.body).len() as i64, count, "GET /boards{query}");
    }
    // The board that is merely *locked* is still open — `?open=` filters
    // `closed_at` and nothing else.
    let res = send(
        &app,
        "PATCH",
        &format!("/boards/{}", boards[1]),
        Some(&ali),
        Some(json!({ "locked": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/boards?open=true&limit=1", Some(&ali), None).await;
    assert_eq!(total(&res.body), 2, "a locked board is still an open one");
    assert_eq!(items(&res.body).len(), 1, "the filtered list still windows");
    assert_eq!(res.body["limit"], 1);
}

/// The class layer's three lists speak the same envelope: the class index, one
/// class's roster and the courses it carries. All three join people onto the
/// page, so the window is checked with the refs still riding on the rows.
#[tokio::test]
async fn class_lists_are_paged() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;

    // Three classes; the first carries three students and three courses.
    let mut classes = Vec::new();
    for n in 0..3 {
        let res = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": format!("9-{n}") })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        classes.push(res.body["class"]["id"].as_str().unwrap().to_string());
    }
    let class = classes[0].clone();
    for n in 0..3 {
        let course = create_course(&app, &manager, &format!("ders{n}")).await;
        let res = send(
            &app,
            "POST",
            &format!("/classes/{class}/courses"),
            Some(&manager),
            Some(json!({ "course_id": course })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

        let student = login(&app, &format!("stud{n}")).await;
        let sid = me_id(&app, &student).await;
        let res = send(
            &app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(&manager),
            Some(json!({ "user_id": sid })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    for uri in [
        "/classes".to_string(),
        format!("/classes/{class}/members"),
        format!("/classes/{class}/courses"),
    ] {
        // Unpaged: everything, `limit` echoes null.
        let res = send(&app, "GET", &uri, Some(&manager), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {uri}: {}", res.body);
        assert_eq!(total(&res.body), 3, "GET {uri} total");
        assert_eq!(items(&res.body).len(), 3, "GET {uri} items");
        assert!(res.body["limit"].is_null(), "GET {uri} echoes a null limit");
        assert_eq!(res.body["offset"], 0);

        // A window of 1: the total stays the unpaged count, consecutive windows
        // are disjoint, and past the end is an empty page rather than an error.
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=1&offset=0"),
            Some(&manager),
            None,
        )
        .await;
        assert_eq!(total(&res.body), 3, "GET {uri} windowed total");
        assert_eq!(items(&res.body).len(), 1, "GET {uri} window size");
        assert_eq!(res.body["limit"], 1);
        let first = items(&res.body)[0]["id"].clone();
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=1&offset=1"),
            Some(&manager),
            None,
        )
        .await;
        assert_eq!(res.body["offset"], 1);
        assert_ne!(items(&res.body)[0]["id"], first, "GET {uri} pages overlap");
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=1&offset=99"),
            Some(&manager),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
        assert!(items(&res.body).is_empty(), "GET {uri} past the end");
        assert_eq!(total(&res.body), 3, "GET {uri} total stays honest");

        // The shared bounds bite on every one of them.
        for bad in ["limit=0", "limit=501", "offset=-1"] {
            let res = send(&app, "GET", &format!("{uri}?{bad}"), Some(&manager), None).await;
            assert_eq!(
                res.status,
                StatusCode::BAD_REQUEST,
                "GET {uri}?{bad} should be 400"
            );
        }
    }

    // The joins run over the page, not the table: each windowed row still
    // carries the people it names.
    let res = send(
        &app,
        "GET",
        "/classes?limit=1&offset=0",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(items(&res.body)[0]["creator"]["username"], "mgr");
    let uri = format!("/classes/{class}/members?limit=1&offset=0");
    let res = send(&app, "GET", &uri, Some(&manager), None).await;
    let row = &items(&res.body)[0];
    assert!(row["user"]["username"].is_string(), "member ref: {row}");
    assert_eq!(row["added_by"]["username"], "mgr");
    let uri = format!("/classes/{class}/courses?limit=1&offset=0");
    let res = send(&app, "GET", &uri, Some(&manager), None).await;
    assert_eq!(items(&res.body)[0]["attached_by"]["username"], "mgr");
}
