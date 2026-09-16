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

/// A plain field PATCH must never touch the course's own membership list — it
/// has its own join/leave path, and a handler that refilled `kind` or the
/// counters from a stale read would silently drop what a concurrent join wrote.
#[tokio::test]
async fn field_patch_leaves_members_alone() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "course_patch_owner", "manager").await;
    let student = login(&app, "course_patch_member").await;
    let student_id = me_id(&app, &student).await;

    let created = send(
        &app,
        "POST",
        "/courses",
        Some(&manager),
        Some(json!({ "title": "robotics club", "kind": "club" })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let id = id_of(&created.body);
    let uri = format!("/courses/{id}");
    let joined = send(
        &app,
        "POST",
        &format!("{uri}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(
        joined.status,
        StatusCode::OK,
        "join member: {}",
        joined.body
    );

    let patched = send(
        &app,
        "PATCH",
        &uri,
        Some(&manager),
        Some(json!({ "title": "renamed" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "title patch");
    assert_eq!(
        patched.body["course_membership_count"], 1,
        "the counter a field PATCH may not clobber: {}",
        patched.body
    );
    let listed = send(&app, "GET", &format!("{uri}/members"), Some(&manager), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(items(&listed.body).len(), 1, "{}", listed.body);
    assert_eq!(
        items(&listed.body)[0]["user"]["id"],
        student_id,
        "the member a plain field PATCH must leave standing: {}",
        listed.body
    );
}
