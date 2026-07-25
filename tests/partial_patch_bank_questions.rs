//! A bank-template PATCH is a read-modify-write: the kind/choices/correct trio
//! has to be re-submitted as a unit (choices carry their ids, and their
//! pictures hang off those ids), so the row read, the merge and the write are
//! held together under the *writer* lease of `BANK_LOCK`. Without that, two
//! concurrent partial PATCHes revert each other.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

async fn template(app: &axum::Router, who: &str, subject: &str) -> String {
    let created = send(
        app,
        "POST",
        "/bank-questions",
        Some(who),
        Some(json!({ "subject_id": subject, "text": "old text",
                     "kind": "text", "points": 1 })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    id_of(&created.body)
}

#[tokio::test]
async fn concurrent_partial_patches_keep_both_fields() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ali", "teacher").await;
    let course = create_course(&app, &who, "math").await;
    let subject = create_subject(&app, &who, &course, "algebra").await;

    for round in 0..20 {
        let uri = format!("/bank-questions/{}", template(&app, &who, &subject).await);

        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "text": "new text" })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "points": 7 })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} text patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} points patch");

        let after = send(&app, "GET", &uri, Some(&who), None).await.body;
        assert_eq!(after["text"], "new text", "round {round}: text reverted");
        assert_eq!(after["points"], 7, "round {round}: points reverted");
    }
}

/// The visibility switch races a text edit: publishing must not be undone by an
/// edit that never mentioned `visibility`, and vice versa.
#[tokio::test]
async fn publishing_survives_a_concurrent_text_edit() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "veli", "teacher").await;
    let course = create_course(&app, &who, "physics").await;
    let subject = create_subject(&app, &who, &course, "optics").await;

    for round in 0..20 {
        let uri = format!("/bank-questions/{}", template(&app, &who, &subject).await);

        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "visibility": "school" })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&who),
                Some(json!({ "text": "edited" })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} publish");
        assert_eq!(b.status, StatusCode::OK, "round {round} text patch");

        let after = send(&app, "GET", &uri, Some(&who), None).await.body;
        assert_eq!(
            after["visibility"], "school",
            "round {round}: publish reverted"
        );
        assert_eq!(after["text"], "edited", "round {round}: text reverted");
    }
}

/// The kind-dependent trio is still merged and re-validated as a unit, and the
/// choice ids (which the pictures hang off) survive a text-only edit.
#[tokio::test]
async fn choice_identity_and_unit_validation_survive() {
    let (app, db) = app_and_db().await;
    let who = login_as(&app, &db, "ayse", "teacher").await;
    let course = create_course(&app, &who, "math").await;
    let subject = create_subject(&app, &who, &course, "algebra").await;

    let created = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&who),
        Some(
            json!({ "subject_id": subject, "text": "2 + 2?", "kind": "choice",
                     "points": 10,
                     "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}],
                     "correct": "c1" }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let ids: Vec<String> = created.body["choices"]
        .as_array()
        .expect("choices")
        .iter()
        .map(|c| c["id"].as_str().expect("choice id").to_string())
        .collect();
    let uri = format!("/bank-questions/{}", id_of(&created.body));

    let edited = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "text": "two plus two?" })),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK);
    let after_ids: Vec<String> = edited.body["choices"]
        .as_array()
        .expect("choices")
        .iter()
        .map(|c| c["id"].as_str().expect("choice id").to_string())
        .collect();
    assert_eq!(after_ids, ids, "a text edit kept every choice id");
    assert_eq!(edited.body["correct"], ids[1], "correct kept");

    // Switching to a text template while keeping the stored choices is still a
    // unit-validation failure — a PATCH cannot land a half-question.
    let bad = send(
        &app,
        "PATCH",
        &uri,
        Some(&who),
        Some(json!({ "kind": "text" })),
    )
    .await;
    assert_eq!(
        bad.status,
        StatusCode::BAD_REQUEST,
        "text kind with stored choices refused"
    );
}
