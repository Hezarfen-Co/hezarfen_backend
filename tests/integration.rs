//! Integration tests: drive the assembled axum router directly (no network)
//! against a fresh in-memory SurrealDB, via `tower::ServiceExt::oneshot`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    app_and_db, create_course, create_exam, enroll, id_of, login, login_as, me_id, mem_app, send,
};
use hezarfen_backend::domain::session::Session;
use hezarfen_backend::domain::user::{Password, User, Username};
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
        ("GET", "/courses"),
        ("POST", "/courses"),
        ("GET", "/courses/me"),
        ("GET", "/marks/me"),
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
async fn exams_are_shared_but_course_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;

    let course_id = create_course(&app, &ali, "algebra").await;
    let ex = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/exams"),
        Some(&ali),
        Some(json!({ "title": "ch3", "description": "algebra", "kind": "quiz", "weight": 2 })),
    )
    .await;
    assert_eq!(ex.status, StatusCode::CREATED);
    assert_eq!(ex.body["kind"], "quiz");
    assert_eq!(ex.body["course"], course_id);
    assert_eq!(ex.body["weight"], 2);
    let exam_id = id_of(&ex.body);

    // Any authenticated user can read the exam and the lists.
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
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/exams"),
            Some(&veli),
            None
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        1
    );

    // A teacher who doesn't manage the course cannot edit or delete its exams.
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

    // Course creator edits (partial) — kind flips homework, title/weight kept.
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
    assert_eq!(res.body["weight"], 2);
}

#[tokio::test]
async fn exam_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let course_id = create_course(&app, &ali, "algebra").await;
    let exams_uri = format!("/courses/{course_id}/exams");

    // Missing/blank title -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"  ","kind":"quiz","weight":1}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Unknown kind -> 400 ("final" is valid now; "essay" is not).
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"t","kind":"essay","weight":1}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"t","kind":"final","weight":1}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    // Weight out of range or missing -> 400 / 422.
    for weight in [json!(0), json!(101), json!(-1)] {
        assert_eq!(
            send(
                &app,
                "POST",
                &exams_uri,
                Some(&ali),
                Some(json!({"title":"t","kind":"quiz","weight":weight}))
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "weight {weight}"
        );
    }
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"t","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing weight is a body schema error"
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
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz", 1).await;

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
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "homework", 1).await;

    // Students cannot create exams, grade anyone, list all results, or delete a result.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/exams"),
            Some(&alice),
            Some(json!({"title":"x","kind":"quiz","weight":1}))
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
    // Target exists but is not enrolled in the course -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({"mark":50,"user_id":bob_id}))
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
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "quiz", 1).await;
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

// --- courses + enrollments + marks ----------------------------------------

#[tokio::test]
async fn courses_are_shared_but_creator_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&ali),
        Some(json!({ "title": "algebra", "description": "numbers" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["title"], "algebra");
    let course_id = id_of(&res.body);

    // Any authenticated user can read the course and the list.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "GET", "/courses", Some(&veli), None)
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
            &format!("/courses/{course_id}"),
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
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Creator edits (partial) — description flips, title kept.
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course_id}"),
        Some(&ali),
        Some(json!({ "description": "letters" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "algebra");
    assert_eq!(res.body["description"], "letters");

    // A manager can edit anyone's course.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/courses/{course_id}"),
            Some(&boss),
            Some(json!({"title":"algebra II"}))
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn course_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    // Missing/blank title -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            "/courses",
            Some(&ali),
            Some(json!({"title":"  "}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Missing course -> 404.
    assert_eq!(
        send(&app, "GET", "/courses/nope", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn enrollment_upsert_roster_and_my_courses() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = me_id(&app, &alice).await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    // Enrolling twice is an upsert: same row id both times.
    let first = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        Some(json!({ "user_id": alice_id })),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let second = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        Some(json!({ "user_id": alice_id })),
    )
    .await;
    assert_eq!(id_of(&first.body), id_of(&second.body), "upsert same row");

    // Roster lists alice once; students can't read the roster.
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(roster.body.as_array().unwrap().len(), 1);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/enrollments"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // /courses/me shows the course for alice, stays empty for bob.
    assert_eq!(
        send(&app, "GET", "/courses/me", Some(&alice), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        send(&app, "GET", "/courses/me", Some(&bob), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // Unenroll -> 204; repeating -> 404.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}/enrollments/{alice_id}"),
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
            "DELETE",
            &format!("/courses/{course_id}/enrollments/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn enrollment_requires_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await; // student
    let alice_id = me_id(&app, &alice).await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    // A teacher who doesn't manage the course cannot enroll anyone.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&veli),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // A manager can.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&boss),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::OK
    );
    // Unknown target -> 400; missing course -> 404.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&teacher),
            Some(json!({ "user_id": "ghost" }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/courses/nope/enrollments",
            Some(&teacher),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn exam_creation_lives_under_courses() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let alice = login(&app, "alice").await; // student

    // The old flat creation route is gone: /exams only serves GET now.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({"title":"t","kind":"quiz","weight":1}))
        )
        .await
        .status,
        StatusCode::METHOD_NOT_ALLOWED
    );

    let course_id = create_course(&app, &teacher, "algebra").await;
    // Creation inside the course echoes course + weight.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/exams"),
        Some(&teacher),
        Some(json!({"title":"mt","kind":"midterm","weight":3})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["course"], course_id);
    assert_eq!(res.body["weight"], 3);

    // A teacher who doesn't manage the course cannot add exams to it.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/exams"),
            Some(&veli),
            Some(json!({"title":"x","kind":"quiz","weight":1}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Any authenticated user (a student) can list the course's exams.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/exams"),
            Some(&alice),
            None
        )
        .await
        .body
        .as_array()
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test]
async fn grading_requires_enrollment() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz", 1).await;
    let grade_uri = format!("/exams/{exam_id}/results");
    let grade_body = json!({ "mark": 70, "user_id": alice_id });

    // Not enrolled -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &grade_uri,
            Some(&teacher),
            Some(grade_body.clone())
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Enrolled -> 200.
    enroll(&app, &teacher, &course_id, &alice_id).await;
    assert_eq!(
        send(
            &app,
            "POST",
            &grade_uri,
            Some(&teacher),
            Some(grade_body.clone())
        )
        .await
        .status,
        StatusCode::OK
    );

    // Unenrolling blocks further grading but keeps the recorded result:
    // the row still lists for the teacher and the student still reads it.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}/enrollments/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, "POST", &grade_uri, Some(&teacher), Some(grade_body))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, "GET", &grade_uri, Some(&teacher), None)
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
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .body["mark"],
        70
    );

    // ...but the mark vanishes from the report until re-enrollment restores it.
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"].as_array().unwrap().len(), 0);
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 70.0);
}

#[tokio::test]
async fn exam_writes_follow_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    // A manager creates an exam in the teacher's course...
    let exam_id = create_exam(&app, &boss, &course_id, "mt", "midterm", 2).await;

    // ...and the course creator (not the exam's creator) can edit and delete it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            Some(json!({"weight":5}))
        )
        .await
        .body["weight"],
        5
    );
    // An unrelated teacher still cannot.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&veli),
            Some(json!({"weight":1}))
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
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn exam_statistics_summarize_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let bob = login(&app, "bob").await;
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    enroll(&app, &teacher, &course_id, &bob_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz", 1).await;
    let stats_uri = format!("/exams/{exam_id}/statistics");

    // Nothing graded yet: zero count, null aggregates.
    let res = send(&app, "GET", &stats_uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["graded"], 0);
    assert_eq!(res.body["average"], serde_json::Value::Null);
    assert_eq!(res.body["min"], serde_json::Value::Null);
    assert_eq!(res.body["max"], serde_json::Value::Null);

    for (id, mark) in [(&alice_id, 60), (&bob_id, 80)] {
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": id })),
        )
        .await;
    }

    let res = send(&app, "GET", &stats_uri, Some(&teacher), None).await;
    assert_eq!(res.body["graded"], 2);
    assert_eq!(res.body["average"], 70.0);
    assert_eq!(res.body["min"], 60);
    assert_eq!(res.body["max"], 80);

    // Students don't see statistics; missing exam is a 404.
    assert_eq!(
        send(&app, "GET", &stats_uri, Some(&alice), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn weighted_averages_follow_exam_weights() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    // Course A: quiz (w1, mark 50) + midterm (w3, mark 90) -> (50 + 270) / 4 = 80.
    let algebra = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &algebra, &alice_id).await;
    let quiz = create_exam(&app, &teacher, &algebra, "quiz", "quiz", 1).await;
    let midterm = create_exam(&app, &teacher, &algebra, "midterm", "midterm", 3).await;
    // A third exam stays ungraded and must not drag the average.
    create_exam(&app, &teacher, &algebra, "final", "final", 4).await;
    for (exam, mark) in [(&quiz, 50), (&midterm, 90)] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": alice_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    // Course B: enrolled, nothing graded -> null average.
    let physics = create_course(&app, &teacher, "physics").await;
    enroll(&app, &teacher, &physics, &alice_id).await;
    create_exam(&app, &teacher, &physics, "hw", "homework", 1).await;

    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.status, StatusCode::OK);
    let courses = report.body["courses"].as_array().unwrap();
    assert_eq!(courses.len(), 2);
    let algebra_block = courses
        .iter()
        .find(|c| c["course"]["title"] == "algebra")
        .unwrap();
    let physics_block = courses
        .iter()
        .find(|c| c["course"]["title"] == "physics")
        .unwrap();

    assert_eq!(algebra_block["average"], 80.0);
    assert_eq!(algebra_block["results"].as_array().unwrap().len(), 2);
    assert_eq!(physics_block["average"], serde_json::Value::Null);
    assert_eq!(physics_block["results"].as_array().unwrap().len(), 0);
    // Overall skips the null course instead of zeroing it.
    assert_eq!(report.body["overall_average"], 80.0);
    assert_eq!(report.body["user"], alice_id);
}

#[tokio::test]
async fn marks_reports_are_self_or_teacher_scoped() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let bob = login(&app, "bob").await;
    let alice_id = me_id(&app, &alice).await;

    // Own report works for a student.
    assert_eq!(
        send(&app, "GET", "/marks/me", Some(&alice), None)
            .await
            .status,
        StatusCode::OK
    );
    // Another user's report needs teacher+.
    assert_eq!(
        send(&app, "GET", &format!("/marks/{alice_id}"), Some(&bob), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/marks/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn deleting_course_cascades_enrollments_exams_and_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz", 1).await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;

    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );

    // Course, exam, result, and enrollment are all gone.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}"),
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
    assert_eq!(
        send(&app, "GET", "/courses/me", Some(&alice), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        send(&app, "GET", "/marks/me", Some(&alice), None)
            .await
            .body["courses"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

/// Same guarantee as attendance/grading: concurrent enrolls of the same pair
/// converge on one row instead of racing the unique index into a 500.
#[tokio::test]
async fn concurrent_enrollments_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    let mut handles = Vec::new();
    for _ in 0..24 {
        let app = app.clone();
        let teacher = teacher.clone();
        let uri = format!("/courses/{course_id}/enrollments");
        let body = json!({ "user_id": alice_id });
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
            "concurrent enroll must not fail"
        );
    }

    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course_id}/enrollments"),
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

// --- users: personal info (profile) ---------------------------------------

#[tokio::test]
async fn profile_starts_empty_and_updates_via_users_me() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    // A fresh account carries no personal info.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert_eq!(me.status, StatusCode::OK);
    for field in ["name", "surname", "email", "phone", "birth_date"] {
        assert!(me.body[field].is_null(), "{field} should start null");
    }

    // Fill everything in via the self-service endpoint.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&bob),
        Some(json!({
            "name": "Bob",
            "surname": "Gümüş",
            "email": "bob@example.com",
            "phone": "+90 555 123 45 67",
            "birth_date": "1990-1-2",
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "Bob");
    assert_eq!(res.body["surname"], "Gümüş"); // unicode names allowed
    assert_eq!(res.body["email"], "bob@example.com");
    assert_eq!(res.body["phone"], "+90 555 123 45 67");
    assert_eq!(res.body["birth_date"], "1990-01-02"); // canonicalized
    assert!(res.body.get("password_hash").is_none());

    // Partial patch: only phone changes, everything else survives.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&bob),
        Some(json!({ "phone": "05551112233" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["phone"], "05551112233");
    assert_eq!(res.body["name"], "Bob");
    assert_eq!(res.body["email"], "bob@example.com");

    // Empty string clears a field; the others stay.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&bob),
        Some(json!({ "phone": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["phone"].is_null());
    assert_eq!(res.body["name"], "Bob");

    // The merged state is what /auth/me reports afterwards.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert_eq!(me.body["name"], "Bob");
    assert_eq!(me.body["surname"], "Gümüş");
    assert_eq!(me.body["birth_date"], "1990-01-02");
    assert!(me.body["phone"].is_null());

    // No session -> 401.
    assert_eq!(
        send(&app, "PATCH", "/users/me", None, Some(json!({"name":"x"})))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn profile_rejects_invalid_fields() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    let bad = [
        json!({ "email": "not-an-email" }),
        json!({ "email": "bob@nodot" }),
        json!({ "phone": "123" }),
        json!({ "phone": "letters" }),
        json!({ "birth_date": "02/01/1990" }),
        json!({ "birth_date": "1990-02-30" }),
        json!({ "birth_date": "9999-01-01" }),
        json!({ "name": "   " }),
        json!({ "name": "x".repeat(101) }),
    ];
    for body in bad {
        let res = send(&app, "PATCH", "/users/me", Some(&bob), Some(body.clone())).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // A rejected patch must not have half-applied: profile is still empty.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    for field in ["name", "surname", "email", "phone", "birth_date"] {
        assert!(me.body[field].is_null(), "{field} should still be null");
    }
}

#[tokio::test]
async fn admin_reads_and_edits_any_profile_with_guards() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "admin", "admin").await;
    let alice = login(&app, "alice").await;
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);
    let admin_id = id_of(&send(&app, "GET", "/auth/me", Some(&admin), None).await.body);

    // Students can neither look up nor edit someone else's record.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/users/{admin_id}"),
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
            "PATCH",
            &format!("/users/{admin_id}/profile"),
            Some(&alice),
            Some(json!({"name":"Mallory"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Admin maintains alice's record on her behalf.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/profile"),
        Some(&admin),
        Some(json!({ "name": "Alice", "email": "alice@school.edu" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "Alice");

    // Admin reads a single user; alice sees the change on /auth/me.
    let one = send(
        &app,
        "GET",
        &format!("/users/{alice_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.body["username"], "alice");
    assert_eq!(one.body["email"], "alice@school.edu");
    let me = send(&app, "GET", "/auth/me", Some(&alice), None).await;
    assert_eq!(me.body["name"], "Alice");

    // The admin listing now carries the info columns too.
    let list = send(&app, "GET", "/users", Some(&admin), None).await;
    let listed = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("alice listed")
        .clone();
    assert_eq!(listed["email"], "alice@school.edu");

    // Unknown ids -> 404 on both admin endpoints.
    assert_eq!(
        send(&app, "GET", "/users/does-not-exist", Some(&admin), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/does-not-exist/profile",
            Some(&admin),
            Some(json!({"name":"Nobody"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
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
    let student_id = me_id(&app, &student).await;
    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &student_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "quiz", 1).await;

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
    let teacher_id = me_id(&app, &teacher).await;
    let boss_id = me_id(&app, &boss).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &teacher_id).await;
    enroll(&app, &teacher, &course_id, &boss_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "t", "quiz", 1).await;

    // Self-grading is forbidden at every privilege level, not just for teachers.
    // (The teacher owns the course; the manager clears the gate by role.)
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
    assert_eq!(
        send(
            &app,
            "GET",
            "/courses/nope/enrollments",
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/courses/nope/exams", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/exams/nope/statistics", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/marks/ghost", Some(&teacher), None)
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

// --- admin seed (ADMIN_USERNAME / ADMIN_PASSWORD bootstrap) ----------------

/// The seed creates a ready-to-use admin: login works with the configured
/// credentials and the account can reach admin-only endpoints immediately.
#[tokio::test]
async fn admin_seed_creates_working_admin() {
    let (app, db) = app_and_db().await;
    let username = Username::try_new("root").unwrap();
    let password = Password::try_new("secret1").unwrap();
    User::ensure_admin(username, password, &db).await.unwrap();

    let creds = json!({ "username": "root", "password": "secret1" });
    let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["role"], "admin");

    let cookie = res.cookie.expect("session cookie");
    let res = send(&app, "GET", "/users", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "admin-only listing must open");
}

/// Re-running the seed (every boot does) must not duplicate the account,
/// demote it, or overwrite its password.
#[tokio::test]
async fn admin_seed_is_idempotent() {
    let (app, db) = app_and_db().await;
    for _ in 0..2 {
        let username = Username::try_new("root").unwrap();
        let password = Password::try_new("secret1").unwrap();
        User::ensure_admin(username, password, &db).await.unwrap();
    }

    let cookie = {
        let creds = json!({ "username": "root", "password": "secret1" });
        let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::OK);
        res.cookie.expect("session cookie")
    };
    let res = send(&app, "GET", "/users", Some(&cookie), None).await;
    let listed = res.body.as_array().expect("user list");
    assert_eq!(listed.len(), 1, "seed must not duplicate the account");
    assert_eq!(listed[0]["role"], "admin");
}

/// A username already registered by someone else is never promoted (or its
/// password replaced) by the seed — that would be privilege escalation.
#[tokio::test]
async fn admin_seed_refuses_existing_non_admin() {
    let (app, db) = app_and_db().await;
    let cookie = login(&app, "squatter").await; // registers with password `secret1`

    let username = Username::try_new("squatter").unwrap();
    let password = Password::try_new("attacker-pw").unwrap();
    User::ensure_admin(username, password, &db).await.unwrap();

    // Still a student: the admin-only listing stays closed...
    let res = send(&app, "GET", "/users", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "must not be promoted");

    // ...and the original password still logs in (nothing was overwritten).
    let creds = json!({ "username": "squatter", "password": "secret1" });
    let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["role"], "student");
}
