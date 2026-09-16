//! Two concurrent partial `PATCH /homework/{id}` requests touching *different*
//! fields must both stick. A handler that fills the omitted fields from the row
//! it read before the write re-sends a stale value, so whichever request lands
//! second silently reverts the other's field — no error, the change is just
//! gone. Looped, because a single round can get lucky on the interleaving.

mod common;

use axum::http::StatusCode;
use common::*;
use hezarfen_backend::domain::timestamp::Timestamp;
use serde_json::json;

const ROUNDS: usize = 20;

/// Set up şube instance + subject + two enrolled students, once per test, plus
/// the teacher who acts on it.
///
/// Homework keys on the class×course *instance*, not the catalog course, so the
/// fixture mints a şube and attaches the course to it. The plain `teacher`
/// cookie acts through the şube's own sınıf öğretmeni seat, which is what
/// [`taught_under`] names them for.
async fn world(
    app: &axum::Router,
    db: &hezarfen_backend::database::Database,
) -> (String, String, String, String) {
    let teacher = login_as(app, db, "teacher", "teacher").await;
    let mudur = login_as(app, db, "mudur", "manager").await;
    let ali = login(app, "ali").await;
    let ali_id = me_id(app, &ali).await;
    let veli = login(app, "veli").await;
    let veli_id = me_id(app, &veli).await;
    let t = taught_under(app, &mudur, &teacher, "math").await;
    let subject = create_subject(app, &teacher, &t.course, "algebra").await;
    enroll(app, &teacher, &t.instance, &ali_id).await;
    enroll(app, &teacher, &t.instance, &veli_id).await;
    (teacher, t.instance, subject, ali_id)
}

/// Title and due date are independent scalar columns: patching one must not
/// roll the other back.
#[tokio::test]
async fn concurrent_title_and_due_at_patches_both_stick() {
    let (app, db) = app_and_db().await;
    let (teacher, instance, subject, _) = world(&app, &db).await;

    for round in 0..ROUNDS {
        let due = Timestamp::now().as_millis() + 3_600_000;
        let id = create_homework(&app, &teacher, &instance, &subject, "hw", due).await;
        let uri = format!("/homework/{id}");
        let new_due = due + 86_400_000;
        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&teacher),
                Some(json!({ "title": "renamed" })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&teacher),
                Some(json!({ "due_at": new_due })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} title patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} due_at patch");

        let after = send(&app, "GET", &uri, Some(&teacher), None).await.body;
        assert_eq!(after["title"], "renamed", "round {round}: title reverted");
        assert_eq!(after["due_at"], new_due, "round {round}: due_at reverted");
    }
}

/// The audience subset is the interesting one: a title-only PATCH carries no
/// `assigned` at all, so it must not write the subset it happened to read —
/// that would silently re-narrow (or re-widen) the audience of a homework
/// whose scope a concurrent PATCH just changed, bypassing the orphan guard.
#[tokio::test]
async fn concurrent_title_and_assigned_patches_both_stick() {
    let (app, db) = app_and_db().await;
    let (teacher, instance, subject, ali_id) = world(&app, &db).await;

    for round in 0..ROUNDS {
        let due = Timestamp::now().as_millis() + 3_600_000;
        let id = create_homework(&app, &teacher, &instance, &subject, "hw", due).await;
        let uri = format!("/homework/{id}");
        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                &uri,
                Some(&teacher),
                Some(json!({ "title": "renamed" })),
            ),
            send(
                &app,
                "PATCH",
                &uri,
                Some(&teacher),
                Some(json!({ "assigned": [ali_id.clone()] })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} title patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} assigned patch");

        let after = send(&app, "GET", &uri, Some(&teacher), None).await.body;
        assert_eq!(after["title"], "renamed", "round {round}: title reverted");
        assert_eq!(
            after["assigned"],
            json!([ali_id]),
            "round {round}: assigned reverted to whole-course"
        );
    }
}

/// `assigned: null` (and `[]`) is an explicit "whole course", not "keep" —
/// the double-option must survive the partial-PATCH conversion, or a subset
/// homework becomes impossible to widen back.
#[tokio::test]
async fn assigned_null_still_clears_the_subset() {
    let (app, db) = app_and_db().await;
    let (teacher, instance, subject, ali_id) = world(&app, &db).await;
    let due = Timestamp::now().as_millis() + 3_600_000;
    let res = create_homework_with(
        &app,
        &teacher,
        &instance,
        json!({ "title": "hw", "subject_id": subject, "due_at": due, "assigned": [ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let uri = format!("/homework/{}", id_of(&res.body));

    // Omitted: keeps the subset.
    let kept = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "title": "kept" })),
    )
    .await;
    assert_eq!(kept.status, StatusCode::OK, "{}", kept.body);
    assert_eq!(kept.body["assigned"], json!([ali_id]), "omitted cleared it");

    // Explicit null: widens back to the whole course.
    let cleared = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "assigned": null })),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert_eq!(
        cleared.body["assigned"],
        json!(null),
        "explicit null did not clear the subset"
    );

    // Same for description: omit keeps, null clears.
    let desc = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "description": "read it" })),
    )
    .await;
    assert_eq!(desc.body["description"], "read it", "{}", desc.body);
    let cleared = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "description": null })),
    )
    .await;
    assert_eq!(cleared.body["description"], json!(null), "{}", cleared.body);
}
