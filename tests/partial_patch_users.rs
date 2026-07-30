//! Two concurrent partial PATCHes on the same user row must not revert each
//! other. The profile and preference handlers merged the request over their own
//! stale read and wrote every column, so a name edit racing a surname edit lost
//! one of them — both 200.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn concurrent_profile_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ali", "student").await;

    for round in 0..20 {
        let name = format!("name{round}");
        let surname = format!("surname{round}");
        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                "/users/me",
                Some(&who),
                Some(json!({ "name": name })),
            ),
            send(
                &app,
                "PATCH",
                "/users/me",
                Some(&who),
                Some(json!({ "surname": surname })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} name patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} surname patch");

        // `/auth/me` is the self-read (there is no `GET /users/me`).
        let after = send(&app, "GET", "/auth/me", Some(&who), None).await.body;
        assert_eq!(after["name"], name, "round {round}: name reverted");
        assert_eq!(after["surname"], surname, "round {round}: surname reverted");
    }
}

#[tokio::test]
async fn concurrent_preference_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "veli", "student").await;

    for round in 0..20 {
        // Alternate both values so a revert to the previous round is visible.
        let (theme, language) = if round % 2 == 0 {
            ("dark", "tr")
        } else {
            ("light", "en")
        };
        let color = if round % 2 == 0 { "#283618" } else { "#fefae0" };
        let (a, b, c) = tokio::join!(
            send(
                &app,
                "PATCH",
                "/users/me/preferences",
                Some(&who),
                Some(json!({ "theme": theme })),
            ),
            send(
                &app,
                "PATCH",
                "/users/me/preferences",
                Some(&who),
                Some(json!({ "language": language })),
            ),
            send(
                &app,
                "PATCH",
                "/users/me/preferences",
                Some(&who),
                Some(json!({ "palette_color": color })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} theme patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} language patch");
        assert_eq!(c.status, StatusCode::OK, "round {round} palette patch");

        let after = send(&app, "GET", "/auth/me", Some(&who), None).await.body;
        assert_eq!(after["theme"], theme, "round {round}: theme reverted");
        assert_eq!(
            after["language"], language,
            "round {round}: language reverted"
        );
        assert_eq!(
            after["palette_color"], color,
            "round {round}: palette_color reverted"
        );
    }
}

/// The field semantics are unchanged: omitted (and `null`) keeps, `""` clears,
/// and anything else runs through the same validators a create would use.
#[tokio::test]
async fn omitted_keeps_empty_clears_and_validation_still_bites() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ayse", "student").await;

    let set = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&who),
        Some(json!({ "name": "Ada", "surname": "Lovelace", "email": "ada@example.com" })),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);

    // Omitted keeps, and an explicit null keeps too (the documented contract).
    let kept = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&who),
        Some(json!({ "surname": null })),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body["name"], "Ada", "omitted name kept");
    assert_eq!(kept.body["surname"], "Lovelace", "null keeps surname");
    assert_eq!(kept.body["email"], "ada@example.com");

    // An empty request writes nothing at all.
    let empty = send(&app, "PATCH", "/users/me", Some(&who), Some(json!({}))).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(empty.body["name"], "Ada");
    assert_eq!(empty.body["email"], "ada@example.com");

    // `""` clears — and only the field it was sent for.
    let cleared = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&who),
        Some(json!({ "email": "" })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.body["email"], json!(null), "email not cleared");
    assert_eq!(cleared.body["name"], "Ada", "name lost to the clear");

    // A malformed value is refused, exactly as at create.
    let bad = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&who),
        Some(json!({ "email": "not-an-email" })),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "bad email refused");

    let bad_theme = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&who),
        Some(json!({ "theme": "chartreuse" })),
    )
    .await;
    assert_eq!(
        bad_theme.status,
        StatusCode::BAD_REQUEST,
        "bad theme refused"
    );

    // The accent color joins them: set, cleared by `""`, refused when malformed
    // — and never touching the other two preferences.
    let set = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&who),
        Some(json!({ "theme": "dark", "palette_color": "#FEFAE0" })),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["palette_color"], "#fefae0", "stored lowercase");

    let cleared = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&who),
        Some(json!({ "palette_color": "" })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.body["palette_color"], json!(null));
    assert_eq!(cleared.body["theme"], "dark", "theme lost to the clear");

    let bad_color = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&who),
        Some(json!({ "palette_color": "#fff" })),
    )
    .await;
    assert_eq!(
        bad_color.status,
        StatusCode::BAD_REQUEST,
        "bad palette color refused"
    );
}
