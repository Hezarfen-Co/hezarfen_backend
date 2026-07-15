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
        let creds = json!({ "username": name, "password": "secret1" });
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
    let res = send(&app, "GET", "/users?limit=10&offset=999", Some(&admin), None).await;
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

#[tokio::test]
async fn user_search_is_paged_not_capped() {
    // `/users/search` used to hard-cap at 10 rows; it now pages like the rest.
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    // 12 accounts sharing the "grp" fragment — past the old cap.
    for i in 0..12 {
        let creds = json!({ "username": format!("grp{i:02}"), "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    // Unpaged: every match comes back — no 10-row ceiling.
    let res = send(&app, "GET", "/users/search?q=grp", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(total(&res.body), 12, "all matches counted, not capped at 10");
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

    // Paging bounds are enforced, and a blank query is still refused.
    let res = send(&app, "GET", "/users/search?q=grp&limit=0", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "limit=0 is a 400");
    let res = send(&app, "GET", "/users/search?q=", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "blank q is still a 400");
}
