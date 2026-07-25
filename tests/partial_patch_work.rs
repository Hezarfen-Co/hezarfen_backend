//! Two concurrent partial PATCHes on the same work entry must not revert each
//! other. A handler that fills omitted fields from its own stale read writes
//! back a value the client never sent, so whichever request lands last wipes
//! the other's correction.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{Value, json};

/// The caller's own stint with id `id`, from the paginated `/work/me` log.
async fn entry(app: &axum::Router, who: &str, id: &str) -> Value {
    let res = send(app, "GET", "/work/me", Some(who), None).await;
    assert_eq!(res.status, StatusCode::OK, "GET /work/me");
    items(&res.body)
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("entry {id} missing from the log"))
        .clone()
}

/// Open then close a stint, returning the closed entry's id.
async fn closed_entry(app: &axum::Router, who: &str) -> String {
    let opened = send(app, "POST", "/work/check-in", Some(who), None).await;
    assert_eq!(opened.status, StatusCode::CREATED, "check in");
    let closed = send(app, "POST", "/work/check-out", Some(who), None).await;
    assert_eq!(closed.status, StatusCode::OK, "check out");
    id_of(&closed.body)
}

#[tokio::test]
async fn concurrent_partial_patches_keep_both_instants() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "mudur", "manager").await;
    // Both corrections straddle the server-stamped pair, so each one is
    // independently valid against the *stored* other side.
    let new_in = 1_000_000_000_000_i64;
    let new_out = 2_000_000_000_000_i64;

    for round in 0..20 {
        let id = closed_entry(&app, &who).await;
        let uri = format!("/work/entries/{id}");

        let a_patch = json!({ "check_in": new_in });
        let b_patch = json!({ "check_out": new_out });
        let (a, b) = tokio::join!(
            send(&app, "PATCH", &uri, Some(&who), Some(a_patch)),
            send(&app, "PATCH", &uri, Some(&who), Some(b_patch)),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} check_in patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} check_out patch");

        let after = entry(&app, &who, &id).await;
        assert_eq!(
            after["check_in"], new_in,
            "round {round}: check_in reverted"
        );
        assert_eq!(
            after["check_out"], new_out,
            "round {round}: check_out reverted"
        );
    }
}

/// `user` and the open/closed bookkeeping stay server-owned: a PATCH body
/// naming them changes nothing.
#[tokio::test]
async fn patch_cannot_reassign_the_stint() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "mudur2", "manager").await;
    let other = login_as(&app, &db, "hoca2", "teacher").await;
    let other_id = me_id(&app, &other).await;
    let mine = me_id(&app, &who).await;

    let id = closed_entry(&app, &who).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{id}"),
        Some(&who),
        Some(json!({ "user": other_id, "id": "hijacked", "check_in": 1_000_000_000_000_i64 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["user"], mine, "user is server-owned");
    assert_eq!(res.body["id"], id, "id is server-owned");
    assert_eq!(res.body["check_in"], 1_000_000_000_000_i64);
}

/// An empty PATCH keeps both instants; an explicit `null` means "keep" too —
/// `check_out` is not clearable here, since an open stint is refused outright.
#[tokio::test]
async fn omitted_and_null_both_keep() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "mudur3", "manager").await;

    let id = closed_entry(&app, &who).await;
    let uri = format!("/work/entries/{id}");
    let before = entry(&app, &who, &id).await;

    for body in [json!({}), json!({ "check_in": null, "check_out": null })] {
        let res = send(&app, "PATCH", &uri, Some(&who), Some(body)).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.body["check_in"], before["check_in"]);
        assert_eq!(res.body["check_out"], before["check_out"]);
    }
}

/// Correcting only one instant is still range-checked against the *stored*
/// other side — reading it for a CHECK is fine, writing it back is not.
#[tokio::test]
async fn range_check_still_sees_the_stored_other_side() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "mudur4", "manager").await;

    let id = closed_entry(&app, &who).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{id}"),
        Some(&who),
        Some(json!({ "check_out": 1_000_000_000_000_i64 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "check_out before the stored check_in"
    );
}
