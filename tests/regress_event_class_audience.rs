//! The `class` event audience (şube): an event aimed at a class section.
//!
//! The rule under test is that the roster resolves LIVE from `class_member`,
//! exactly as `course` resolves from `enrollment` and `role` from the user row
//! — never snapshotted at create time. The class layer's own membership stays
//! real rows written by the pump; nothing here touches that.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{id_of, items, login_as, send, total};
use hezarfen_backend::database::Database;
use serde_json::json;

async fn create_class(app: &Router, cookie: &str, name: &str) -> String {
    let res = send(
        app,
        "POST",
        "/classes",
        Some(cookie),
        Some(json!({"name": name})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    id_of(&res.body["class"])
}

/// An event aimed at `class`, created by a teacher+.
async fn class_event(app: &Router, cookie: &str, class: &str) -> String {
    let res = send(
        app,
        "POST",
        "/events",
        Some(cookie),
        Some(json!({"title": "Veli toplantısı", "audience": {"kind": "class", "class": class}})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    id_of(&res.body)
}

/// The event's live roster, as user ids.
async fn roster(app: &Router, cookie: &str, event: &str) -> Vec<String> {
    let res = send(
        app,
        "GET",
        &format!("/events/{event}/roster"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(total(&res.body), items(&res.body).len() as i64);
    items(&res.body)
        .iter()
        .map(|entry| entry["user"]["id"].as_str().unwrap().to_string())
        .collect()
}

async fn student(app: &Router, db: &Database, username: &str) -> (String, String) {
    let cookie = login_as(app, db, username, "student").await;
    let id = common::me_id(app, &cookie).await;
    (cookie, id)
}

async fn add_member(app: &Router, cookie: &str, class: &str, user: &str) -> StatusCode {
    send(
        app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(cookie),
        Some(json!({"user_id": user})),
    )
    .await
    .status
}

/// (a) the roster is exactly the class's current members, and (b) a student
/// added *after* the event was created is on it — the live-resolution proof.
#[tokio::test]
async fn class_roster_resolves_live() {
    let (app, db) = common::app_and_db().await;
    let boss = login_as(&app, &db, "mudur", "manager").await;
    let (_, ayse) = student(&app, &db, "ayse").await;
    let (_, mehmet) = student(&app, &db, "mehmet").await;
    let (_, outsider) = student(&app, &db, "kenan").await;

    let class = create_class(&app, &boss, "9-A").await;
    assert_eq!(
        add_member(&app, &boss, &class, &ayse).await,
        StatusCode::CREATED
    );

    let event = class_event(&app, &boss, &class).await;
    assert_eq!(roster(&app, &boss, &event).await, vec![ayse.clone()]);

    // Added after the event exists: a frozen roster would miss them.
    assert_eq!(
        add_member(&app, &boss, &class, &mehmet).await,
        StatusCode::CREATED
    );
    let mut live = roster(&app, &boss, &event).await;
    live.sort();
    let mut expected = vec![ayse.clone(), mehmet.clone()];
    expected.sort();
    assert_eq!(
        live, expected,
        "the roster must follow the class, not a snapshot"
    );
    assert!(!live.contains(&outsider), "a non-member must never appear");

    // ...and removal takes them straight back off it.
    let removed = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/members/{mehmet}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{:?}", removed.body);
    assert_eq!(roster(&app, &boss, &event).await, vec![ayse.clone()]);

    // The point check behind marking agrees with the roster: a member can be
    // marked, an outsider is refused.
    for (user, expect) in [
        (&ayse, StatusCode::OK),
        (&outsider, StatusCode::BAD_REQUEST),
    ] {
        let marked = send(
            &app,
            "POST",
            &format!("/events/{event}/attendance"),
            Some(&boss),
            Some(json!({"status": "present", "user_id": user})),
        )
        .await;
        assert_eq!(marked.status, expect, "{user}: {:?}", marked.body);
    }
}

/// (c) deleting the class leaves the event alive with an empty roster — the
/// behaviour a deleted *course* audience already has (`Course::delete` never
/// touches `event`), copied deliberately.
#[tokio::test]
async fn deleting_the_class_empties_the_roster_without_erroring() {
    let (app, db) = common::app_and_db().await;
    let boss = login_as(&app, &db, "mudur", "manager").await;
    let (_, ayse) = student(&app, &db, "ayse").await;

    let class = create_class(&app, &boss, "9-A").await;
    assert_eq!(
        add_member(&app, &boss, &class, &ayse).await,
        StatusCode::CREATED
    );
    let event = class_event(&app, &boss, &class).await;
    assert_eq!(roster(&app, &boss, &event).await, vec![ayse.clone()]);

    // `ClassGroup::delete` refuses a class that still holds members, so the
    // roster is emptied first — the only route by which a class can go away.
    let held = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(held.status, StatusCode::CONFLICT, "{:?}", held.body);
    send(
        &app,
        "DELETE",
        &format!("/classes/{class}/members/{ayse}"),
        Some(&boss),
        None,
    )
    .await;
    let gone = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{:?}", gone.body);

    // The event survives its class, pointing at a record that no longer exists.
    let read = send(&app, "GET", &format!("/events/{event}"), Some(&boss), None).await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
    assert_eq!(read.body["audience"]["kind"], "class");
    assert_eq!(
        roster(&app, &boss, &event).await,
        Vec::<String>::new(),
        "a dangling class resolves empty, it does not error"
    );
}

/// (d) the DTO round-trips: what create echoes, what a read echoes, and what a
/// PATCH onto and off the kind stores.
#[tokio::test]
async fn the_audience_dto_round_trips() {
    let (app, db) = common::app_and_db().await;
    let boss = login_as(&app, &db, "mudur", "manager").await;
    let class = create_class(&app, &boss, "9-A").await;

    let created = send(
        &app,
        "POST",
        "/events",
        Some(&boss),
        Some(json!({"title": "Gezi", "audience": {"kind": "class", "class": class}})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let wire = json!({"kind": "class", "class": class});
    assert_eq!(created.body["audience"], wire);
    let event = id_of(&created.body);

    // Re-read off the stored row: SCHEMAFULL must have kept `audience.class`.
    let read = send(&app, "GET", &format!("/events/{event}"), Some(&boss), None).await;
    assert_eq!(read.body["audience"], wire);

    // PATCH onto the kind from another one, and back off it again.
    let school = send(
        &app,
        "PATCH",
        &format!("/events/{event}"),
        Some(&boss),
        Some(json!({"audience": {"kind": "school"}})),
    )
    .await;
    assert_eq!(school.body["audience"], json!({"kind": "school"}));
    let back = send(
        &app,
        "PATCH",
        &format!("/events/{event}"),
        Some(&boss),
        Some(json!({"audience": {"kind": "class", "class": class}})),
    )
    .await;
    assert_eq!(back.status, StatusCode::OK, "{:?}", back.body);
    assert_eq!(back.body["audience"], wire);

    // An unknown class is a 400, not a stored dangling reference.
    let bogus = send(
        &app,
        "POST",
        "/events",
        Some(&boss),
        Some(json!({"title": "Yok", "audience": {"kind": "class", "class": "01JNOPE"}})),
    )
    .await;
    assert_eq!(bogus.status, StatusCode::BAD_REQUEST, "{:?}", bogus.body);

    // A class-audience event takes no registrations — the two kinds are
    // exclusive, so no signup row can collide with a class roster.
    let seat = send(
        &app,
        "POST",
        &format!("/events/{event}/register"),
        Some(&boss),
        Some(json!({})),
    )
    .await;
    assert_eq!(seat.status, StatusCode::BAD_REQUEST, "{:?}", seat.body);
}
