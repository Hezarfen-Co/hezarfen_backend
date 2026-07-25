//! Two concurrent partial PATCHes on the same subject must not revert each
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
    let course = create_course(&app, &who, "math").await;

    for round in 0..20 {
        let created = send(
            &app,
            "POST",
            &format!("/courses/{course}/subjects"),
            Some(&who),
            Some(json!({ "name": "old name", "description": "old description" })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "round {round} create");
        let uri = format!("/subjects/{}", id_of(&created.body));

        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "name": "new name" })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "description": "new description" })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} name patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} description patch");

        let after = send(&app, "GET", &uri, Some(&who), None).await.body;
        assert_eq!(after["name"], "new name", "round {round}: name reverted");
        assert_eq!(
            after["description"], "new description",
            "round {round}: description reverted"
        );
    }
}

/// Omitting a field keeps it, and a PATCH still cannot land a value a POST
/// would reject.
#[tokio::test]
async fn omitted_keeps_and_validation_still_bites() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "veli", "teacher").await;
    let course = create_course(&app, &who, "physics").await;
    let subject = create_subject(&app, &who, &course, "kept name").await;
    let uri = format!("/subjects/{subject}");

    let kept = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "description": "only this" })),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body["name"], "kept name", "omitted name kept");
    assert_eq!(kept.body["description"], "only this");

    // An empty PATCH writes nothing at all and reads the row back unchanged.
    let empty = send(&app, "PATCH", &uri, Some(&who), Some(json!({}))).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(empty.body["name"], "kept name");
    assert_eq!(empty.body["description"], "only this");

    // A blank name is required at create, so it stays refused at patch.
    let bad = send(&app, "PATCH", &uri, Some(&who), Some(json!({ "name": "" }))).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "blank name refused");
}
