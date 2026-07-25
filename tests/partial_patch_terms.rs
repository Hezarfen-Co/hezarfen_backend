//! Two concurrent partial PATCHes on the same term must not revert each
//! other: each writes only the field it carried, so `name` and `ends_at`
//! survive together no matter which write lands last.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn concurrent_partial_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "term_race_manager", "manager").await;

    for round in 0..20 {
        let created = send(
            &app,
            "POST",
            "/terms",
            Some(&manager),
            Some(json!({
                "name": "before",
                "starts_at": 1_780_000_000_000_i64,
                "ends_at": 1_790_000_000_000_i64,
            })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "round {round} create");
        let id = id_of(&created.body);
        let uri = format!("/terms/{id}");

        let name_patch = json!({ "name": "after" });
        let ends_patch = json!({ "ends_at": 1_795_000_000_000_i64 });
        let (a, b) = tokio::join!(
            send(&app, "PATCH", &uri, Some(&manager), Some(name_patch)),
            send(&app, "PATCH", &uri, Some(&manager), Some(ends_patch)),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} name patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} ends_at patch");

        let after = send(&app, "GET", &uri, Some(&manager), None).await.body;
        assert_eq!(after["name"], "after", "round {round}: name reverted");
        assert_eq!(
            after["ends_at"], 1_795_000_000_000_i64,
            "round {round}: ends_at reverted"
        );
    }
}

/// The range check is cross-field, so a partial PATCH validates the arriving
/// end against the *stored* other end. Two PATCHes that are each fine against
/// the stored row must not be able to commit an inverted range between them.
#[tokio::test]
async fn concurrent_range_patches_never_invert_the_term() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "term_range_manager", "manager").await;

    for round in 0..20 {
        let created = send(
            &app,
            "POST",
            "/terms",
            Some(&manager),
            Some(json!({
                "name": "range",
                "starts_at": 1_000_000_000_000_i64,
                "ends_at": 2_000_000_000_000_i64,
            })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "round {round} create");
        let uri = format!("/terms/{}", id_of(&created.body));

        // Each is valid against the stored row on its own: 1.9e12 < 2e12 and
        // 1.1e12 > 1e12. Together they would leave starts_at > ends_at.
        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&manager),
                Some(json!({ "starts_at": 1_900_000_000_000_i64 })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&manager),
                Some(json!({ "ends_at": 1_100_000_000_000_i64 })),
            ),
        );
        assert!(
            a.status == StatusCode::OK || a.status == StatusCode::BAD_REQUEST,
            "round {round} starts_at patch: {}",
            a.status
        );
        assert!(
            b.status == StatusCode::OK || b.status == StatusCode::BAD_REQUEST,
            "round {round} ends_at patch: {}",
            b.status
        );

        let after = send(&app, "GET", &uri, Some(&manager), None).await.body;
        let starts = after["starts_at"].as_i64().expect("starts_at");
        let ends = after["ends_at"].as_i64().expect("ends_at");
        assert!(
            starts <= ends,
            "round {round}: stored range inverted — starts_at {starts} > ends_at {ends}"
        );
    }
}
