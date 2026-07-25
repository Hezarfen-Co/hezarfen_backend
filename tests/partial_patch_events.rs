//! Two concurrent partial PATCHes on the same event must not revert each
//! other. A handler that fills omitted fields from its own stale read writes
//! back a value the client never sent, so whichever request lands last wipes
//! the other's field.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn concurrent_partial_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ali", "teacher").await;

    for round in 0..20 {
        let created = send(
            &app,
            "POST",
            "/events",
            Some(&who),
            Some(json!({ "title": "old title", "description": "old description" })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "round {round} create");
        let id = id_of(&created.body);
        let uri = format!("/events/{id}");

        let a_patch = json!({ "title": "new title" });
        let b_patch = json!({ "description": "new description" });
        let (a, b) = tokio::join!(
            send(&app, "PATCH", &uri, Some(&who), Some(a_patch)),
            send(&app, "PATCH", &uri, Some(&who), Some(b_patch)),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} title patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} description patch");

        let after = send(&app, "GET", &uri, Some(&who), None).await.body;
        assert_eq!(after["title"], "new title", "round {round}: title reverted");
        assert_eq!(
            after["description"], "new description",
            "round {round}: description reverted"
        );
    }
}

/// The schedule columns are genuinely nullable: an explicit `null` must still
/// clear them, and an omitted one must still keep them.
#[tokio::test]
async fn schedule_stays_clearable_and_keepable() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "veli", "teacher").await;
    let future = 1_900_000_000_000_i64;

    let created = send(
        &app,
        "POST",
        "/events",
        Some(&who),
        Some(json!({ "title": "timed", "starts_at": future, "ends_at": future + 3_600_000 })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let uri = format!("/events/{}", id_of(&created.body));

    // Omitted: both kept.
    let kept = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "title": "t" })),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body["starts_at"], future);
    assert_eq!(kept.body["ends_at"], future + 3_600_000);

    // Explicit null: cleared.
    let cleared = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "starts_at": null, "ends_at": null })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.body["starts_at"], json!(null));
    assert_eq!(cleared.body["ends_at"], json!(null));
}

/// Setting only `ends_at` is still range-checked against the *stored*
/// `starts_at` — reading the other side for a CHECK is fine, writing it back
/// is not.
#[tokio::test]
async fn range_check_still_sees_the_stored_other_side() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ayse", "teacher").await;
    let future = 1_900_000_000_000_i64;

    let created = send(
        &app,
        "POST",
        "/events",
        Some(&who),
        Some(json!({ "title": "timed", "starts_at": future })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let uri = format!("/events/{}", id_of(&created.body));

    let bad = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "ends_at": future - 1000 })),
    )
    .await;
    assert_eq!(
        bad.status,
        StatusCode::BAD_REQUEST,
        "ends_at before stored starts_at"
    );
}
