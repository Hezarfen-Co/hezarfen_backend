//! `POST /auth/register` must not tell an unauthenticated caller whether a
//! username exists — not by status, not by body, not by how long it takes.

mod common;

use std::time::Instant;

use axum::http::StatusCode;
use common::{app_and_db, items, send, set_role};
use serde_json::json;

#[tokio::test]
async fn duplicate_register_is_indistinguishable_from_a_fresh_one() {
    let (app, db) = app_and_db().await;
    let taken = json!({ "username": "ada", "password": "secret1" });

    let first = send(&app, "POST", "/auth/register", None, Some(taken.clone())).await;
    assert_eq!(first.status, StatusCode::CREATED);

    // Same username again, different password: still 201, same body shape.
    let dup_creds = json!({ "username": "ada", "password": "attacker" });
    let started = Instant::now();
    let dup = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(dup_creds.clone()),
    )
    .await;
    let dup_elapsed = started.elapsed();

    let fresh = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": "grace", "password": "secret1" })),
    )
    .await;

    assert_eq!(dup.status, fresh.status, "status leaks existence");
    assert_eq!(dup.status, StatusCode::CREATED);

    // Structurally identical: same keys, and every value either the echoed
    // username or the same default. Only `id` may differ (it is a ULID).
    let dup_obj = dup.body.as_object().expect("dup body object");
    let fresh_obj = fresh.body.as_object().expect("fresh body object");
    assert_eq!(
        dup_obj.keys().collect::<Vec<_>>(),
        fresh_obj.keys().collect::<Vec<_>>(),
        "field set leaks existence"
    );
    for (key, value) in dup_obj {
        if key == "id" || key == "username" {
            continue;
        }
        assert_eq!(value, &fresh_obj[key], "field {key} leaks existence");
    }
    assert_eq!(dup_obj["username"], "ada");
    assert_eq!(dup_obj["role"], "student");
    assert_eq!(
        dup_obj["id"].as_str().map(str::len),
        fresh_obj["id"].as_str().map(str::len),
        "id shape leaks existence"
    );
    assert!(dup.cookie.is_none() && fresh.cookie.is_none());

    // The argon2 work is what equalises timing, so pin that the taken path pays
    // it. A floor, not a comparison between the two paths — a gap assertion
    // would be flaky, whereas this can only fail if hashing got skipped
    // (argon2's default cost is ~33ms; 8ms leaves room for a loaded CI box).
    assert!(
        dup_elapsed.as_millis() >= 8,
        "taken-username register returned in {dup_elapsed:?} — it skipped argon2, \
         which re-opens the timing oracle"
    );

    // The original account is untouched: its password still works and the
    // attacker's does not.
    let login = send(&app, "POST", "/auth/login", None, Some(taken)).await;
    assert_eq!(
        login.status,
        StatusCode::OK,
        "original password stopped working"
    );
    let hijack = send(&app, "POST", "/auth/login", None, Some(dup_creds)).await;
    assert_eq!(
        hijack.status,
        StatusCode::UNAUTHORIZED,
        "the duplicate register overwrote the password"
    );

    // And no second row was written: exactly two users exist (ada, grace).
    set_role(&db, "ada", "admin").await;
    let cookie = login.cookie.expect("session cookie");
    let list = send(&app, "GET", "/users", Some(&cookie), None).await;
    assert_eq!(list.status, StatusCode::OK);
    let names: Vec<&str> = items(&list.body)
        .iter()
        .map(|u| u["username"].as_str().expect("username"))
        .collect();
    assert_eq!(
        names.iter().filter(|n| **n == "ada").count(),
        1,
        "duplicate register created a second row: {names:?}"
    );
    assert_eq!(names.len(), 2, "unexpected user rows: {names:?}");
}
