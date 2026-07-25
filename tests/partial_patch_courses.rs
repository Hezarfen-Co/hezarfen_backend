//! Two concurrent partial PATCHes of *different* course fields must both
//! survive. A handler that refills omitted fields from its own stale read
//! reverts whatever the other writer just committed — the loop makes the
//! interleaving reliable rather than lucky.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn concurrent_partial_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "course_patch_teacher", "teacher").await;

    for round in 0..20 {
        let id = create_course(&app, &teacher, &format!("original {round}")).await;
        let uri = format!("/courses/{id}");
        let title = format!("renamed {round}");
        let description = format!("described {round}");
        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&teacher),
                Some(json!({ "title": title })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&teacher),
                Some(json!({ "description": description })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} title patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} description patch");

        let after = send(&app, "GET", &uri, Some(&teacher), None).await.body;
        assert_eq!(after["title"], title, "round {round}: title reverted");
        assert_eq!(
            after["description"], description,
            "round {round}: description reverted"
        );
    }
}

/// `capacity` and `term` are nullable, so an explicit `null` must still clear
/// them — the absent-vs-null split must not make them unclearable.
#[tokio::test]
async fn capacity_and_term_stay_clearable() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "course_patch_manager", "manager").await;

    let term = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({
            "name": "2026 spring",
            "starts_at": 1_780_000_000_000_i64,
            "ends_at": 1_790_000_000_000_i64,
        })),
    )
    .await;
    assert_eq!(term.status, StatusCode::CREATED, "create term");
    let term_id = id_of(&term.body);

    let id = create_course(&app, &manager, "clearable").await;
    let uri = format!("/courses/{id}");

    let set = send(
        &app,
        "PATCH",
        &uri,
        Some(&manager),
        Some(json!({ "capacity": 12, "term_id": term_id })),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "set capacity and term");
    assert_eq!(set.body["capacity"], 12);
    assert_eq!(set.body["term"], term_id);

    // An omitted field keeps its value...
    let kept = send(
        &app,
        "PATCH",
        &uri,
        Some(&manager),
        Some(json!({ "title": "still capped" })),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK, "omit keeps");
    assert_eq!(kept.body["capacity"], 12);
    assert_eq!(kept.body["term"], term_id);

    // ...an explicit null clears it.
    let cleared = send(
        &app,
        "PATCH",
        &uri,
        Some(&manager),
        Some(json!({ "capacity": null, "term_id": null })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "clear capacity and term");
    assert!(cleared.body["capacity"].is_null(), "capacity not cleared");
    assert!(cleared.body["term"].is_null(), "term not cleared");
}

/// A plain field PATCH must never touch `teachers[]` — it has its own
/// assign/unassign path.
#[tokio::test]
async fn field_patch_leaves_teachers_alone() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "course_patch_owner", "manager").await;
    let assistant = login_as(&app, &db, "course_patch_assistant", "teacher").await;
    let assistant_id = me_id(&app, &assistant).await;

    let id = create_course(&app, &manager, "team taught").await;
    let uri = format!("/courses/{id}");
    let assigned = send(
        &app,
        "POST",
        &format!("{uri}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": assistant_id })),
    )
    .await;
    assert_eq!(assigned.status, StatusCode::OK, "assign teacher");

    let patched = send(
        &app,
        "PATCH",
        &uri,
        Some(&manager),
        Some(json!({ "title": "renamed" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "title patch");
    assert_eq!(patched.body["teachers"].as_array().unwrap().len(), 1);
    assert_eq!(patched.body["teachers"][0]["id"], assistant_id);
}
