//! Integration tests: drive the assembled axum router directly (no network)
//! against a fresh in-memory SurrealDB, via `tower::ServiceExt::oneshot`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app_and_db, id_of, login, login_as, mem_app, send};
use hezarfen_backend::domain::session::Session;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::json;
use tower::ServiceExt;

// --- health + auth -------------------------------------------------------

#[tokio::test]
async fn health_reports_ok() {
    let app = mem_app().await;
    let res = send(&app, "GET", "/health", None, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "ok");
}

#[tokio::test]
async fn register_validates_input() {
    let app = mem_app().await;

    // Password too short -> 400.
    let short = json!({ "username": "bob", "password": "123" });
    let res = send(&app, "POST", "/auth/register", None, Some(short)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Blank username -> 400.
    let blank = json!({ "username": "   ", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(blank)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Valid -> 201, no password echoed back, defaults to the student role.
    let ok = json!({ "username": "bob", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(ok.clone())).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "bob");
    assert_eq!(res.body["role"], "student");
    assert!(res.body.get("password_hash").is_none());

    // Duplicate username -> 409.
    let res = send(&app, "POST", "/auth/register", None, Some(ok)).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn login_rejects_bad_credentials() {
    let app = mem_app().await;
    send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": "kate", "password": "secret1" })),
    )
    .await;

    // Wrong password.
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": "kate", "password": "wrong" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    // Unknown user.
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": "ghost", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_routes_require_session() {
    let app = mem_app().await;
    for (method, uri) in [
        ("GET", "/auth/me"),
        ("GET", "/notes"),
        ("POST", "/notes"),
        ("GET", "/events"),
        ("POST", "/events"),
        ("GET", "/exams"),
        ("POST", "/exams"),
    ] {
        let res = send(&app, method, uri, None, None).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
}

#[tokio::test]
async fn garbage_cookie_is_unauthorized() {
    let app = mem_app().await;
    let res = send(&app, "GET", "/auth/me", Some("session=deadbeef"), None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_returns_current_user_without_secrets() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let res = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["username"], "ali");
    assert_eq!(res.body["role"], "student");
    assert!(res.body.get("password_hash").is_none());
}

#[tokio::test]
async fn logout_invalidates_the_session_server_side() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;

    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&ali), None).await.status,
        StatusCode::OK
    );

    let res = send(&app, "POST", "/auth/logout", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    // Re-using the same token now fails: the session row is gone.
    let res = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

// --- notes ---------------------------------------------------------------

#[tokio::test]
async fn notes_crud_is_scoped_to_owner() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "first", "content": "hello" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let note_id = id_of(&res.body);

    // Partial update keeps the untouched field.
    let res = send(
        &app,
        "PATCH",
        &format!("/notes/{note_id}"),
        Some(&ali),
        Some(json!({ "content": "edited" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["content"], "edited");
    assert_eq!(res.body["title"], "first");

    // Owner sees exactly one; other user sees none and gets 404 on direct access.
    assert_eq!(
        send(&app, "GET", "/notes", Some(&ali), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&veli), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/notes", Some(&veli), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // Other user cannot update or delete it either.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/notes/{note_id}"),
            Some(&veli),
            Some(json!({"title":"x"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Delete, then it's gone.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn note_content_defaults_to_empty() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "solo" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["content"], "");
}

#[tokio::test]
async fn note_validation_and_missing_ids() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;

    // Blank title on create -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            "/notes",
            Some(&ali),
            Some(json!({ "title": "  " }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Operations on a non-existent id -> 404.
    for method in ["GET", "PATCH", "DELETE"] {
        let body = (method == "PATCH").then(|| json!({ "title": "x" }));
        let res = send(&app, method, "/notes/does-not-exist", Some(&ali), body).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method}");
    }

    // Blank title on update -> 400.
    let created = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "keep" })),
    )
    .await;
    let id = id_of(&created.body);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/notes/{id}"),
            Some(&ali),
            Some(json!({ "title": "" }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
}

// --- events + attendance -------------------------------------------------

#[tokio::test]
async fn events_are_shared_but_creator_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;

    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "standup", "description": "daily" })),
    )
    .await;
    assert_eq!(ev.status, StatusCode::CREATED);
    let event_id = id_of(&ev.body);

    // Any authenticated user can read the event and its roster.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "GET", "/events", Some(&veli), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Non-creator cannot edit or delete.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&veli),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Creator can edit (partial).
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "description": "moved" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["description"], "moved");
    assert_eq!(res.body["title"], "standup");

    // Blank title on update -> 400. Missing event -> 404.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"title":" "}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, "GET", "/events/nope", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn attendance_marking_and_upsert() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = id_of(&send(&app, "GET", "/auth/me", Some(&veli), None).await.body);

    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "standup" })),
        )
        .await
        .body,
    );

    // Default target is the caller.
    let res = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "present" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "present");

    // Mark veli, then re-mark: upsert keeps a single row (same id).
    let first = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "late", "user_id": veli_id })),
    )
    .await;
    let second = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "absent", "user_id": veli_id })),
    )
    .await;
    assert_eq!(
        id_of(&first.body),
        id_of(&second.body),
        "upsert same record"
    );
    assert_eq!(second.body["status"], "absent");

    // Two rows total.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            None
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        2
    );

    // Remove veli -> one row; removing again -> 404.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}/attendance/{veli_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            None
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}/attendance/{veli_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn attendance_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "e" })),
        )
        .await
        .body,
    );

    // Invalid status -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            Some(json!({"status":"maybe"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Unknown target user -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            Some(json!({"status":"present","user_id":"ghost"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Attendance on a missing event -> 404.
    assert_eq!(
        send(
            &app,
            "POST",
            "/events/nope/attendance",
            Some(&ali),
            Some(json!({"status":"present"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_event_cascades_attendance() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "e" })),
        )
        .await
        .body,
    );
    send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({"status":"present"})),
    )
    .await;

    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    // Event gone — and its roster endpoint with it (404, not an empty list).
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn event_timestamps_echo_and_validate() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    // Create with unix-millis times.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "mtg", "starts_at": 1000, "ends_at": 2000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["starts_at"], 1000);
    assert_eq!(res.body["ends_at"], 2000);
    let event_id = id_of(&res.body);

    // Omitted times serialize as null.
    let untimed = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "notime" })),
    )
    .await;
    assert!(untimed.body["starts_at"].is_null());
    assert!(untimed.body["ends_at"].is_null());

    // ends_at before starts_at -> 400.
    let bad = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "bad", "starts_at": 5000, "ends_at": 1000 })),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    // Updating only ends_at keeps starts_at.
    let updated = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "ends_at": 3000 })),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["starts_at"], 1000);
    assert_eq!(updated.body["ends_at"], 3000);
}

// --- exams + results -----------------------------------------------------

#[tokio::test]
async fn exams_are_shared_but_creator_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;

    let ex = send(
        &app,
        "POST",
        "/exams",
        Some(&ali),
        Some(json!({ "title": "ch3", "description": "algebra", "kind": "quiz" })),
    )
    .await;
    assert_eq!(ex.status, StatusCode::CREATED);
    assert_eq!(ex.body["kind"], "quiz");
    let exam_id = id_of(&ex.body);

    // Any authenticated user can read the exam and the list.
    assert_eq!(
        send(&app, "GET", &format!("/exams/{exam_id}"), Some(&veli), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "GET", "/exams", Some(&veli), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Non-creator teacher cannot edit or delete.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&veli),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Creator edits (partial) — kind flips homework, title kept.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam_id}"),
        Some(&ali),
        Some(json!({ "kind": "homework" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["kind"], "homework");
    assert_eq!(res.body["title"], "ch3");
}

#[tokio::test]
async fn exam_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    // Missing/blank title -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams",
            Some(&ali),
            Some(json!({"title":"  ","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Unknown kind -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams",
            Some(&ali),
            Some(json!({"title":"t","kind":"final"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Missing exam -> 404.
    assert_eq!(
        send(&app, "GET", "/exams/nope", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn grading_upsert_and_own_result() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);

    let exam_id = id_of(
        &send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({"title":"mt","kind":"quiz"})),
        )
        .await
        .body,
    );

    // Before grading, the student's own result is 404.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Teacher grades alice, then re-grades: upsert keeps one row (same id).
    let first = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let second = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 88, "user_id": alice_id })),
    )
    .await;
    assert_eq!(
        id_of(&first.body),
        id_of(&second.body),
        "upsert same record"
    );
    assert_eq!(second.body["mark"], 88);

    // Alice reads her own result; bob (ungraded) still 404s and can't see alice's.
    let mine = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}/result"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(mine.status, StatusCode::OK);
    assert_eq!(mine.body["mark"], 88);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&bob),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Teacher sees the full list (one row).
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            None
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        1
    );

    // Remove alice's result -> her own read 404s again; removing again -> 404.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}/results/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}/results/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn grading_rbac_and_validation() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);

    let exam_id = id_of(
        &send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({"title":"e","kind":"homework"})),
        )
        .await
        .body,
    );

    // Students cannot create exams, grade anyone, list all results, or delete a result.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams",
            Some(&alice),
            Some(json!({"title":"x","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&alice),
            Some(json!({"mark":100,"user_id":alice_id}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/results"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}/results/{alice_id}"),
            Some(&bob),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Mark out of range -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({"mark":101,"user_id":alice_id}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Unknown target user -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({"mark":50,"user_id":"ghost"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Grading on a missing exam -> 404.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams/nope/results",
            Some(&teacher),
            Some(json!({"mark":50,"user_id":alice_id}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_exam_cascades_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);

    let exam_id = id_of(
        &send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({"title":"e","kind":"quiz"})),
        )
        .await
        .body,
    );
    send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({"mark":60,"user_id":alice_id})),
    )
    .await;

    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    // Exam gone; the student's result is gone with it.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

// --- roles / RBAC --------------------------------------------------------

#[tokio::test]
async fn students_cannot_create_events() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await; // default role: student
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "class" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn students_mark_only_themselves() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let bob_id = id_of(&send(&app, "GET", "/auth/me", Some(&bob), None).await.body);

    let ev = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&teacher),
            Some(json!({ "title": "class" })),
        )
        .await
        .body,
    );

    // A student may mark their own attendance.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{ev}/attendance"),
            Some(&alice),
            Some(json!({"status":"present"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    // But not someone else's.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{ev}/attendance"),
            Some(&alice),
            Some(json!({"status":"present","user_id":bob_id}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // And cannot remove attendance rows.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{ev}/attendance/{bob_id}"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn event_management_follows_hierarchy() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let other = login_as(&app, &db, "other", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;

    let ev = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&owner),
            Some(json!({ "title": "orig" })),
        )
        .await
        .body,
    );

    // A different teacher (not the creator) cannot edit it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{ev}"),
            Some(&other),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // A manager can edit anyone's event.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{ev}"),
        Some(&boss),
        Some(json!({"title":"by mgr"})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "by mgr");
    // The creator can still delete their own.
    assert_eq!(
        send(&app, "DELETE", &format!("/events/{ev}"), Some(&owner), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn admin_manages_roles_with_guards() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "admin", "admin").await;
    let alice = login(&app, "alice").await; // student
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);

    // Non-admins cannot reach the role endpoints.
    assert_eq!(
        send(&app, "GET", "/users", Some(&alice), None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{alice_id}/role"),
            Some(&alice),
            Some(json!({"role":"teacher"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Admin lists users (at least admin + alice).
    let list = send(&app, "GET", "/users", Some(&admin), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(list.body.as_array().unwrap().len() >= 2);

    // Admin promotes alice to teacher; the change is reflected in the response...
    let promoted = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/role"),
        Some(&admin),
        Some(json!({"role":"teacher"})),
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK);
    assert_eq!(promoted.body["role"], "teacher");
    // ...and takes effect immediately: alice can now create events.
    assert_eq!(
        send(
            &app,
            "POST",
            "/events",
            Some(&alice),
            Some(json!({ "title": "now allowed" }))
        )
        .await
        .status,
        StatusCode::CREATED
    );

    // Unknown role value -> 400.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{alice_id}/role"),
            Some(&admin),
            Some(json!({"role":"wizard"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Missing user -> 404.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/does-not-exist/role",
            Some(&admin),
            Some(json!({"role":"teacher"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // An admin cannot change their own role (anti-lockout guard).
    let admin_id = id_of(&send(&app, "GET", "/auth/me", Some(&admin), None).await.body);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{admin_id}/role"),
            Some(&admin),
            Some(json!({"role":"student"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

// --- regression: race safety, session hygiene, CORS ----------------------

/// #1 — Many concurrent marks for the *same* (event, user). A find-then-insert
/// implementation races the unique index and 500s; the atomic composite-id
/// upsert must let every request succeed and leave exactly one row.
#[tokio::test]
async fn concurrent_attendance_marks_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "student").await;
    let student_id = id_of(
        &send(&app, "GET", "/auth/me", Some(&student), None)
            .await
            .body,
    );
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&teacher),
            Some(json!({ "title": "e" })),
        )
        .await
        .body,
    );

    let mut handles = Vec::new();
    for i in 0..24 {
        let app = app.clone();
        let teacher = teacher.clone();
        let uri = format!("/events/{event_id}/attendance");
        let status = if i % 2 == 0 { "present" } else { "late" };
        let body = json!({ "status": status, "user_id": student_id });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent mark must not fail"
        );
    }

    let roster = send(
        &app,
        "GET",
        &format!("/events/{event_id}/attendance"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        roster.body.as_array().unwrap().len(),
        1,
        "upsert keeps a single row"
    );
}

/// #1 — Same guarantee for exam grading.
#[tokio::test]
async fn concurrent_exam_grades_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "student").await;
    let student_id = id_of(
        &send(&app, "GET", "/auth/me", Some(&student), None)
            .await
            .body,
    );
    let exam_id = id_of(
        &send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({ "title": "e", "kind": "quiz" })),
        )
        .await
        .body,
    );

    let mut handles = Vec::new();
    for mark in 0..24i64 {
        let app = app.clone();
        let teacher = teacher.clone();
        let uri = format!("/exams/{exam_id}/results");
        let body = json!({ "mark": mark, "user_id": student_id });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent grade must not fail"
        );
    }

    let results = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        results.body.as_array().unwrap().len(),
        1,
        "upsert keeps a single row"
    );
}

/// #2 — A successful login sweeps expired session rows but leaves live ones.
#[tokio::test]
async fn login_purges_expired_sessions() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // one live session

    // Inject a session that has already expired.
    db.query("CREATE session SET user = type::record('user', $u), token = 'expired-token', expires_at = 1")
        .bind(("u", "nobody".to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        Session::find_by_token("expired-token", &db)
            .await
            .unwrap()
            .is_some()
    );

    // A later successful login sweeps expired rows...
    login(&app, "veli").await;
    assert!(
        Session::find_by_token("expired-token", &db)
            .await
            .unwrap()
            .is_none(),
        "expired session should be purged on login"
    );
    // ...while the still-valid session keeps working.
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&ali), None).await.status,
        StatusCode::OK
    );
}

/// Regression: `Username::try_new` used to validate the trimmed value but store
/// the raw input, so `"ali"`, `"ali "` and `" ali"` were three distinct,
/// visually identical accounts — and a user who registered with padding could
/// only log in by reproducing it exactly. The name must be canonicalized
/// (trimmed) at construction and at login lookup.
#[tokio::test]
async fn padded_usernames_are_canonicalized_not_distinct_accounts() {
    let app = mem_app().await;

    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": "ali", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Padding must not mint a lookalike account.
    for spoof in ["ali ", " ali", "  ali  "] {
        let res = send(
            &app,
            "POST",
            "/auth/register",
            None,
            Some(json!({ "username": spoof, "password": "secret1" })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::CONFLICT,
            "{spoof:?} must collide with \"ali\""
        );
    }

    // A name registered with padding is stored trimmed...
    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": " veli ", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "veli");

    // ...and the login lookup tolerates padding the same way registration does.
    for attempt in ["veli", " veli "] {
        let res = send(
            &app,
            "POST",
            "/auth/login",
            None,
            Some(json!({ "username": attempt, "password": "secret1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "login as {attempt:?}");
    }
}

/// Regression: the grade endpoint verified the exam, the mark, and that the
/// target exists — but never that the grader isn't the target, so any teacher
/// could write their own mark. "Grading never targets oneself" (README).
#[tokio::test]
async fn graders_cannot_grade_themselves() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let teacher_id = id_of(
        &send(&app, "GET", "/auth/me", Some(&teacher), None)
            .await
            .body,
    );
    let boss_id = id_of(&send(&app, "GET", "/auth/me", Some(&boss), None).await.body);

    let exam_id = id_of(
        &send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({"title":"t","kind":"quiz"})),
        )
        .await
        .body,
    );

    // Self-grading is forbidden at every privilege level, not just for teachers.
    for (cookie, own_id) in [(&teacher, &teacher_id), (&boss, &boss_id)] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(cookie),
            Some(json!({ "mark": 100, "user_id": own_id })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "self-grade must be rejected"
        );
    }

    // And the rejected attempts wrote nothing.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            None
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        0
    );

    // Grading someone *else* still works (boss grades teacher).
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&boss),
        Some(json!({ "mark": 90, "user_id": teacher_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// Regression: `User::create` pre-checks the username and then inserts, so two
/// concurrent registrations of the same name could both pass the check; the
/// loser then hit the unique index and surfaced as a raw 500. A lost race must
/// report the same 409 as the sequential duplicate, and exactly one account may
/// exist afterwards.
#[tokio::test]
async fn concurrent_duplicate_registrations_conflict_not_500() {
    let (app, db) = app_and_db().await;

    let mut handles = Vec::new();
    for _ in 0..24 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            send(
                &app,
                "POST",
                "/auth/register",
                None,
                Some(json!({ "username": "dup", "password": "secret1" })),
            )
            .await
            .status
        }));
    }

    let mut created = 0;
    for h in handles {
        match h.await.unwrap() {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {}
            other => panic!("unexpected status {other} — a lost race must be a 409, not a 500"),
        }
    }
    assert_eq!(created, 1, "exactly one registration wins");

    // Exactly one row exists for the name.
    let users: Vec<hezarfen_backend::domain::user::User> = db
        .query("SELECT * FROM user WHERE username = 'dup'")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(
        users.len(),
        1,
        "exactly one user row for the duplicated name"
    );
}

/// Regression: listing a child collection of a missing parent used to return an
/// empty 200 list — indistinguishable from "exists but empty". A missing parent
/// is a 404, matching the write endpoints on the same paths.
#[tokio::test]
async fn child_lists_of_missing_parents_are_404() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;

    assert_eq!(
        send(&app, "GET", "/events/nope/attendance", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/exams/nope/results", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// Regression: the session cookie never carried the `Secure` attribute, so a
/// TLS deployment would let browsers replay it over plain HTTP. The attribute
/// now follows `COOKIE_SECURE` (`Config::cookie_secure`); the default stays off
/// so plain-HTTP local dev keeps working.
#[tokio::test]
async fn session_cookie_secure_attribute_follows_config() {
    async fn login_set_cookie(app: &axum::Router) -> String {
        let creds = json!({ "username": "ada", "password": "secret1" });
        assert_eq!(
            send(app, "POST", "/auth/register", None, Some(creds.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let req = Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(creds.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        res.headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    // Default (dev): HttpOnly but not Secure.
    let plain = login_set_cookie(&mem_app().await).await;
    assert!(
        plain.contains("HttpOnly"),
        "cookie must stay HttpOnly: {plain}"
    );
    assert!(
        !plain.contains("Secure"),
        "dev cookie must not be Secure: {plain}"
    );

    // With the flag on: Secure present.
    let db = database::init_mem().await.unwrap();
    let app = build_router(AppState {
        db,
        cookie_secure: true,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
    });
    let secure = login_set_cookie(&app).await;
    assert!(
        secure.contains("Secure"),
        "cookie must be Secure when configured: {secure}"
    );
    assert!(
        secure.contains("HttpOnly"),
        "Secure must not displace HttpOnly: {secure}"
    );
}

/// Regression: PATCH could set an event's times but never clear them — a JSON
/// `null` was indistinguishable from an omitted field and silently kept the old
/// value. An explicit `null` now clears the field; omitted still keeps it.
#[tokio::test]
async fn patch_null_clears_event_times() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({"title":"mtg","starts_at":1000,"ends_at":2000})),
        )
        .await
        .body,
    );

    // Omitted fields still keep their values.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({"title":"mtg2"})),
    )
    .await;
    assert_eq!(res.body["starts_at"], 1000);
    assert_eq!(res.body["ends_at"], 2000);

    // An explicit null clears just that field.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({"ends_at": null})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["starts_at"], 1000);
    assert!(res.body["ends_at"].is_null());

    // With ends_at cleared, moving starts_at past the old end is legal...
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"starts_at": 5000}))
        )
        .await
        .status,
        StatusCode::OK
    );
    // ...and a new ends_at before starts_at is still rejected.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"ends_at": 1}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Clearing both works.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({"starts_at": null, "ends_at": null})),
    )
    .await;
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
}

/// #4 — CORS echoes the caller's origin and allows credentials, never `*`
/// (which browsers reject for cookie-bearing requests).
#[tokio::test]
async fn cors_is_credential_safe_not_wildcard() {
    let app = mem_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("origin", "https://client.example")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let headers = res.headers();
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://client.example",
        "origin must be reflected, not wildcarded"
    );
    assert_eq!(
        headers.get("access-control-allow-credentials").unwrap(),
        "true",
    );
}
