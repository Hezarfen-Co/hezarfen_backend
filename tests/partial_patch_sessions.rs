//! Two concurrent partial PATCHes on the same session must not revert each
//! other, and — since only the fields a request carried are written — the
//! `starts_at`/`ends_at` pair must not be invertible by two one-ended PATCHes
//! either (that is what `FieldUpdate::ordered`'s in-statement range guard is
//! for).

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

const FUTURE: i64 = 1_900_000_000_000;

#[tokio::test]
async fn concurrent_partial_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ali", "teacher").await;
    let course = create_course(&app, &who, "math").await;

    for round in 0..20 {
        let created = send(
            &app,
            "POST",
            &format!("/courses/{course}/sessions"),
            Some(&who),
            Some(json!({ "topic": "old topic", "starts_at": FUTURE })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "round {round} create");
        let uri = format!("/sessions/{}", id_of(&created.body));

        // `topic` races `ends_at`: different fields, neither may lose.
        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "topic": "new topic" })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "ends_at": FUTURE + 3_600_000 })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} topic patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} ends_at patch");

        let after = send(&app, "GET", &uri, Some(&who), None).await.body;
        assert_eq!(after["topic"], "new topic", "round {round}: topic reverted");
        assert_eq!(
            after["ends_at"],
            FUTURE + 3_600_000,
            "round {round}: ends_at reverted"
        );
    }
}

/// Each end moved *just inside* the other is valid on its own snapshot, so two
/// concurrent one-ended PATCHes could each pass their check against the stored
/// row and still commit an inverted lesson between them. Whatever each request
/// returns (200/400 are both fine), the stored pair must stay ordered.
#[tokio::test]
async fn concurrent_range_patches_never_invert_the_stored_row() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "range", "teacher").await;
    let course = create_course(&app, &who, "math").await;
    let ends = FUTURE + 3_600_000;

    for round in 0..20 {
        let created = send(
            &app,
            "POST",
            &format!("/courses/{course}/sessions"),
            Some(&who),
            Some(json!({ "starts_at": FUTURE, "ends_at": ends })),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "round {round} create");
        let uri = format!("/sessions/{}", id_of(&created.body));

        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "starts_at": ends - 1000 })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "ends_at": FUTURE + 1000 })),
            ),
        );

        let after = send(&app, "GET", &uri, Some(&who), None).await.body;
        let stored_starts = after["starts_at"].as_i64().expect("starts_at");
        let stored_ends = after["ends_at"].as_i64().expect("ends_at");
        assert!(
            stored_starts <= stored_ends,
            "round {round}: stored inverted lesson starts_at={stored_starts} \
             ends_at={stored_ends} (statuses {} {})",
            a.status,
            b.status
        );
    }
}

/// `ends_at` stays genuinely nullable: omitted keeps it, an explicit `null`
/// clears it — and a one-ended PATCH is still checked against the stored other
/// end (read for the check, never written back).
#[tokio::test]
async fn ends_at_stays_clearable_and_range_checked() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "veli", "teacher").await;
    let course = create_course(&app, &who, "physics").await;
    let session = create_session(&app, &who, &course, FUTURE).await;
    let uri = format!("/sessions/{session}");

    let set = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "ends_at": FUTURE + 1_000 })),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK);
    assert_eq!(set.body["ends_at"], FUTURE + 1_000);

    let kept = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "topic": "kept ends" })),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body["starts_at"], FUTURE, "omitted starts_at kept");
    assert_eq!(kept.body["ends_at"], FUTURE + 1_000, "omitted ends_at kept");

    let bad = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "ends_at": FUTURE - 1_000 })),
    )
    .await;
    assert_eq!(
        bad.status,
        StatusCode::BAD_REQUEST,
        "ends_at before stored starts_at"
    );

    let cleared = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "ends_at": null })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.body["ends_at"], json!(null), "ends_at not cleared");
}
