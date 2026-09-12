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

/// Archiving a term freezes it: the term itself takes no PATCH or DELETE, and
/// no new course or class may link it. Reads stay open — a past year is
/// read-only, not hidden.
#[tokio::test]
async fn an_archived_term_is_frozen_for_writes_and_open_for_reads() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "archive_manager", "manager").await;
    let teacher = login_as(&app, &db, "archive_teacher", "teacher").await;
    let student = login_as(&app, &db, "archive_student", "student").await;

    let created = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({
            "name": "2024",
            "starts_at": 1_700_000_000_000_i64,
            "ends_at": 1_710_000_000_000_i64,
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert!(created.body["archived_at"].is_null(), "born open");
    let id = id_of(&created.body);
    let uri = format!("/terms/{id}");
    let archive = format!("/terms/{id}/archive");

    // Only manager+ may flip it.
    for (who, cookie) in [("teacher", &teacher), ("student", &student)] {
        let res = send(&app, "POST", &archive, Some(cookie), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{who} archive");
    }

    let archived = send(&app, "POST", &archive, Some(&manager), None).await;
    assert_eq!(archived.status, StatusCode::OK);
    let stamp = archived.body["archived_at"].as_i64().expect("archived_at");

    // Idempotent: a repeat keeps the original stamp, the year is not re-dated.
    let again = send(&app, "POST", &archive, Some(&manager), None).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(
        again.body["archived_at"].as_i64(),
        Some(stamp),
        "re-stamped"
    );

    // Frozen: edit and delete both refuse with the coded 409.
    for (method, body) in [
        ("PATCH", Some(json!({ "name": "renamed" }))),
        ("DELETE", None),
    ] {
        let res = send(&app, method, &uri, Some(&manager), body).await;
        assert_eq!(
            res.status,
            StatusCode::CONFLICT,
            "{method} on archived term"
        );
        assert_eq!(res.body["code"], "term_archived", "{method} code");
    }

    // No new structure may be hung on a past year — one refusal per link site.
    let new_course = send(
        &app,
        "POST",
        "/courses",
        Some(&manager),
        Some(json!({ "title": "algebra", "term_id": id })),
    )
    .await;
    assert_eq!(new_course.status, StatusCode::CONFLICT, "course link");
    assert_eq!(new_course.body["code"], "term_archived");
    let new_class = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "term_id": id })),
    )
    .await;
    assert_eq!(new_class.status, StatusCode::CONFLICT, "class link");
    assert_eq!(new_class.body["code"], "term_archived");

    // Reads keep working and carry the stamp, single and listed.
    let one = send(&app, "GET", &uri, Some(&student), None).await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.body["archived_at"].as_i64(), Some(stamp));
    let listed = send(&app, "GET", "/terms", Some(&student), None).await;
    assert_eq!(listed.status, StatusCode::OK);
    let row = listed.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("the archived term is still listed");
    assert_eq!(row["archived_at"].as_i64(), Some(stamp));

    // Re-opening thaws it, and is idempotent too.
    let unarchive = format!("/terms/{id}/unarchive");
    let reopened = send(&app, "POST", &unarchive, Some(&manager), None).await;
    assert_eq!(reopened.status, StatusCode::OK);
    assert!(reopened.body["archived_at"].is_null(), "reopened");
    let twice = send(&app, "POST", &unarchive, Some(&manager), None).await;
    assert_eq!(twice.status, StatusCode::OK);
    assert!(twice.body["archived_at"].is_null());
    let patched = send(
        &app,
        "PATCH",
        &uri,
        Some(&manager),
        Some(json!({ "name": "renamed" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "open again, edits land");

    // An id nobody minted is a 404 on both flips, not a 409.
    for route in ["/terms/nope/archive", "/terms/nope/unarchive"] {
        let res = send(&app, "POST", route, Some(&manager), None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{route}");
    }
}

/// A term row with no `archived_at` (NULL = open) must decode and stay
/// archivable — the row a pre-port database arrives with is not special.
#[tokio::test]
async fn a_pre_migration_term_row_still_decodes_as_open() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "archive_legacy_manager", "manager").await;

    let legacy = hezarfen_backend::domain::term::TermId::generate();
    sqlx::query("INSERT INTO term (id, name, starts_at, ends_at) VALUES ($1, 'old', 100, 200)")
        .bind(legacy)
        .execute(&db)
        .await
        .expect("legacy term query");

    let res = send(
        &app,
        "GET",
        &format!("/terms/{}", legacy.key()),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "pre-migration row must decode");
    assert_eq!(res.body["name"], "old");
    assert!(res.body["archived_at"].is_null(), "absent = open");

    // And it is still writable, archiving included.
    let archived = send(
        &app,
        "POST",
        &format!("/terms/{}/archive", legacy.key()),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK);
    assert!(archived.body["archived_at"].as_i64().is_some());
}
