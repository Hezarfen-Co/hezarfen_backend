//! Integration tests: drive the assembled axum router directly (no network)
//! against a fresh in-memory SurrealDB, via `tower::ServiceExt::oneshot`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    app_and_db, create_course, create_exam, create_exam_with, create_session, enroll, id_of, login,
    login_as, me_id, mem_app, send,
};
use hezarfen_backend::domain::exam::ExamId;
use hezarfen_backend::domain::exam_attempt::ExamAttempt;
use hezarfen_backend::domain::session::Session;
use hezarfen_backend::domain::timestamp::Timestamp;
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
async fn time_serves_server_clock_without_auth() {
    let app = mem_app().await;

    let before = Timestamp::now().as_millis();
    let res = send(&app, "GET", "/time", None, None).await;
    let after = Timestamp::now().as_millis();

    assert_eq!(res.status, StatusCode::OK);
    // Bracketed by two local reads: proves it is the same clock in millis,
    // not seconds/nanos or some other epoch.
    let now = res.body["now"].as_i64().expect("now is an integer");
    assert!(before <= now && now <= after);
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
        ("POST", "/exams/x/attempt"),
        ("GET", "/exams/x/attempt"),
        ("POST", "/exams/x/attempt/finish"),
        ("GET", "/exams/x/live"),
        ("GET", "/exams/x/live/stream"),
        ("POST", "/exams/x/questions"),
        ("GET", "/exams/x/questions"),
        ("PATCH", "/exams/x/questions/y"),
        ("DELETE", "/exams/x/questions/y"),
        ("GET", "/exams/x/attempt/questions"),
        ("POST", "/exams/x/attempt/answers"),
        ("GET", "/exams/x/attempts/u/answers"),
        // The WebSocket room authenticates before it upgrades.
        ("GET", "/exams/x/attempt/ws"),
        ("POST", "/courses/x/sessions"),
        ("GET", "/courses/x/sessions"),
        ("GET", "/sessions/x"),
        ("PATCH", "/sessions/x"),
        ("DELETE", "/sessions/x"),
        ("POST", "/sessions/x/attendance"),
        ("GET", "/sessions/x/attendance"),
        ("DELETE", "/sessions/x/attendance/u"),
        ("POST", "/work/check-in"),
        ("POST", "/work/check-out"),
        ("GET", "/work/me"),
        ("GET", "/work/u"),
        ("PATCH", "/work/entries/x"),
        ("DELETE", "/work/entries/x"),
        ("GET", "/attendance/me"),
        ("GET", "/attendance/u"),
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
    let now = Timestamp::now().as_millis();
    let starts = now + 3_600_000;
    let ends = now + 7_200_000;

    // Create with unix-millis times.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "mtg", "starts_at": starts, "ends_at": ends })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["starts_at"], starts);
    assert_eq!(res.body["ends_at"], ends);
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
        Some(json!({ "title": "bad", "starts_at": now + 5_000_000, "ends_at": now + 1_000_000 })),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    // Past times -> 400, whichever end carries them.
    for body in [
        json!({ "title": "old", "starts_at": now - 3_600_000 }),
        json!({ "title": "old", "ends_at": now - 3_600_000 }),
    ] {
        let res = send(&app, "POST", "/events", Some(&ali), Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }

    // Updating only ends_at keeps starts_at.
    let updated = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "ends_at": now + 10_800_000 })),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["starts_at"], starts);
    assert_eq!(updated.body["ends_at"], now + 10_800_000);

    // A PATCH may not move a time into the past either — whichever end.
    for body in [
        json!({ "starts_at": now - 3_600_000 }),
        json!({ "ends_at": now - 3_600_000 }),
    ] {
        let res = send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }
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

/// An expired session row is rejected at auth even while it still exists —
/// expiry must not depend on the purge sweep having run.
#[tokio::test]
async fn expired_session_is_unauthorized_before_any_purge() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;

    // Copy ali's live session into a second row that expired long ago
    // (epoch millis 1), pointing at the same real user.
    db.query(
        "CREATE session SET \
            user = (SELECT VALUE user FROM ONLY session WHERE token = $live LIMIT 1), \
            token = 'stale-token', expires_at = 1",
    )
    .bind(("live", ali.trim_start_matches("session=").to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();

    // Row exists, but the clock says no.
    assert!(
        Session::find_by_token("stale-token", &db)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        send(&app, "GET", "/auth/me", Some("session=stale-token"), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
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
    let now = Timestamp::now().as_millis();
    let starts = now + 3_600_000;
    let ends = now + 7_200_000;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({"title":"mtg","starts_at": starts,"ends_at": ends})),
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
    assert_eq!(res.body["starts_at"], starts);
    assert_eq!(res.body["ends_at"], ends);

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
    assert_eq!(res.body["starts_at"], starts);
    assert!(res.body["ends_at"].is_null());

    // With ends_at cleared, moving starts_at past the old end is legal...
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"starts_at": now + 9_000_000}))
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
            Some(json!({"ends_at": now + 1_000_000}))
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

// --- scheduled exams: sync/async attempts + the live monitor ---------------

/// Create a scheduled exam from a full body and return its id (asserts 201).
async fn scheduled_exam(
    app: &axum::Router,
    cookie: &str,
    course: &str,
    body: serde_json::Value,
) -> String {
    let res = create_exam_with(app, cookie, course, body).await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create scheduled exam: {}",
        res.body
    );
    id_of(&res.body)
}

#[tokio::test]
async fn exam_scheduling_validates_and_echoes() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sched_t", "teacher").await;
    let course = create_course(&app, &teacher, "algebra").await;
    let now = Timestamp::now().as_millis();

    // Sync: the window is echoed, no duration.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "midterm", "kind": "midterm", "weight": 2,
            "mode": "sync", "starts_at": now + 60_000, "ends_at": now + 120_000,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "sync");
    assert_eq!(res.body["starts_at"].as_i64(), Some(now + 60_000));
    assert_eq!(res.body["ends_at"].as_i64(), Some(now + 120_000));
    assert!(res.body["duration_ms"].is_null());

    // Async: adds the per-student duration.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "takehome", "kind": "quiz", "weight": 1,
            "mode": "async", "starts_at": now, "ends_at": now + 7_200_000,
            "duration_ms": 5_400_000,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "async");
    assert_eq!(res.body["duration_ms"].as_i64(), Some(5_400_000));

    // Unscheduled: every schedule field stays null (pre-schedule behavior).
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "homework", "kind": "homework", "weight": 1 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["mode"].is_null());
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
    assert!(res.body["duration_ms"].is_null());

    // Inconsistent schedules are rejected as a unit.
    for (label, body) in [
        (
            "unknown mode",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "weekly",
                    "starts_at": now, "ends_at": now + 1_000 }),
        ),
        (
            "times without mode",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "starts_at": now }),
        ),
        (
            "duration without mode",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "duration_ms": 60_000 }),
        ),
        (
            "sync missing ends_at",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "sync",
                    "starts_at": now }),
        ),
        (
            "sync with duration",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "sync",
                    "starts_at": now, "ends_at": now + 1_000, "duration_ms": 60_000 }),
        ),
        (
            "async without duration",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "async",
                    "starts_at": now, "ends_at": now + 1_000 }),
        ),
        (
            "backwards window",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "sync",
                    "starts_at": now + 2_000, "ends_at": now + 1_000 }),
        ),
        (
            "empty window",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "sync",
                    "starts_at": now, "ends_at": now }),
        ),
        (
            "duration too short",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "async",
                    "starts_at": now, "ends_at": now + 90_000_000, "duration_ms": 59_999 }),
        ),
        (
            "duration too long",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "async",
                    "starts_at": now, "ends_at": now + 90_000_000, "duration_ms": 86_400_001 }),
        ),
        (
            "past starts_at",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "sync",
                    "starts_at": now - 3_600_000, "ends_at": now + 3_600_000 }),
        ),
        (
            // starts_at is future so the rejection can only come from ends_at.
            "past ends_at",
            json!({ "title": "x", "kind": "quiz", "weight": 1, "mode": "sync",
                    "starts_at": now + 3_600_000, "ends_at": now - 3_600_000 }),
        ),
    ] {
        let res = create_exam_with(&app, &teacher, &course, body).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }
}

/// Nothing can be scheduled to start (or end) in the past — on create and on
/// any PATCH that sets a time. Values a PATCH merely keeps are exempt, so a
/// running exam (its `starts_at` naturally behind the clock) stays editable.
#[tokio::test]
async fn schedule_times_cannot_be_backdated() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "past_t", "teacher").await;
    let course = create_course(&app, &teacher, "history").await;
    let now = Timestamp::now().as_millis();

    // A running exam: started seconds ago (inside the clock-skew grace).
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "running", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;

    // Setting either time to the past is rejected...
    for body in [
        json!({ "starts_at": now - 3_600_000 }),
        json!({ "ends_at": now - 3_600_000 }),
    ] {
        let res = send(
            &app,
            "PATCH",
            &format!("/exams/{exam}"),
            Some(&teacher),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }

    // ...while edits that only keep the stored (already past) start work:
    // a rename, and a deadline extension.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "title": "renamed" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "ends_at": now + 1_200_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["ends_at"].as_i64(), Some(now + 1_200_000));

    // The grace is real: times well inside it (30s < 60s) are accepted, on
    // create and on PATCH alike.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "graced", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 30_000, "ends_at": now + 600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let graced = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{graced}"),
        Some(&teacher),
        Some(json!({ "starts_at": now - 20_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn sync_attempt_lifecycle_feeds_the_live_monitor() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "live_t", "teacher").await;
    let ayse = login(&app, "ayse").await;
    let veli = login(&app, "veli").await;
    let ayse_id = me_id(&app, &ayse).await;
    let veli_id = me_id(&app, &veli).await;
    let course = create_course(&app, &teacher, "physics").await;
    enroll(&app, &teacher, &course, &ayse_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;

    let now = Timestamp::now().as_millis();
    let ends = now + 600_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final", "weight": 3,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends }),
    )
    .await;

    // No attempt yet.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Start: 201, in progress, and (sync) the deadline is the window close.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress");
    assert_eq!(res.body["user"]["username"], "veli");
    assert_eq!(res.body["deadline"].as_i64(), Some(ends));
    assert!(res.body["mark"].is_null());
    let started_at = res.body["started_at"].as_i64().expect("started_at");
    let remaining = res.body["remaining_ms"].as_i64().expect("remaining_ms");
    assert!(
        remaining > 0 && remaining <= 601_000,
        "remaining {remaining}"
    );

    // Re-"start" resumes the same attempt with the original clock: 200.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["started_at"].as_i64(), Some(started_at));

    // The monitor sees one writer and one absentee, sorted by username.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["enrolled"], 2);
    assert_eq!(res.body["counts"]["in_progress"], 1);
    assert_eq!(res.body["counts"]["not_started"], 1);
    assert!(res.body["now"].as_i64().is_some());
    let students = res.body["students"].as_array().expect("students");
    assert_eq!(students[0]["user"]["username"], "ayse");
    assert_eq!(students[0]["status"], "not_started");
    assert!(students[0]["started_at"].is_null());
    assert!(students[0]["deadline"].is_null());
    assert_eq!(students[1]["user"]["username"], "veli");
    assert_eq!(students[1]["status"], "in_progress");
    assert!(students[1]["remaining_ms"].as_i64().expect("remaining") > 0);

    // Submit, then grade mid-session; both land in the next snapshot.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "submitted");
    assert!(res.body["finished_at"].as_i64().is_some());
    assert!(res.body["remaining_ms"].is_null());

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 85, "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["counts"]["submitted"], 1);
    assert_eq!(res.body["counts"]["graded"], 1);
    assert_eq!(res.body["students"][1]["mark"], 85);

    // The student's own view carries the mark; the monitor stays teacher+.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.body["status"], "submitted");
    assert_eq!(res.body["mark"], 85);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn async_deadline_is_start_plus_duration_clamped_to_window() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "async_t", "teacher").await;
    let student = login(&app, "asli").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "chem").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // Roomy window: the personal duration is the binding constraint.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "quiz", "weight": 1,
                "mode": "async", "starts_at": now - 1_000, "ends_at": now + 6_000_000,
                "duration_ms": 60_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let started = res.body["started_at"].as_i64().expect("started_at");
    assert_eq!(res.body["deadline"].as_i64(), Some(started + 60_000));

    // Tight window: `ends_at` clamps the personal deadline.
    let ends = now + 30_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz2", "kind": "quiz", "weight": 1,
                "mode": "async", "starts_at": now - 1_000, "ends_at": ends,
                "duration_ms": 60_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["deadline"].as_i64(), Some(ends));
}

#[tokio::test]
async fn attempts_gate_on_schedule_enrollment_and_window() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "gate_t", "teacher").await;
    let student = login(&app, "gita").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "bio").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // Unscheduled exams cannot be sat at all.
    let unscheduled = create_exam(&app, &teacher, &course, "hw", "homework", 1).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{unscheduled}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Not-yet-open and already-closed windows are conflicts too.
    let future = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "later", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now + 600_000, "ends_at": now + 1_200_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{future}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    let past = scheduled_exam(
        &app,
        &teacher,
        &course,
        // Inside the backdating grace, yet already closed by the clock.
        json!({ "title": "gone", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 50_000, "ends_at": now - 10_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{past}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Enrollment is required — teachers included (the monitor is their view).
    let open = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "open", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let outsider = login(&app, "omer").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{open}/attempt"),
        Some(&outsider),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{open}/attempt"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A missing exam is a 404, not a conflict.
    let res = send(
        &app,
        "POST",
        "/exams/does-not-exist/attempt",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn finish_rejects_double_submit_missing_attempt_and_expiry() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "fin_t", "teacher").await;
    let student = login(&app, "fern").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "cs").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;

    // Finishing before starting: no attempt row to close.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "double submit");

    // Expiry: a window that closes right after the start. Real time, not a
    // mocked clock — the server judges expiry by `Timestamp::now()`.
    let brief = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "blitz", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000,
                "ends_at": Timestamp::now().as_millis() + 1_500 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{brief}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress");

    tokio::time::sleep(std::time::Duration::from_millis(1_700)).await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{brief}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{brief}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["status"], "expired");
    assert!(res.body["finished_at"].is_null());
    assert!(res.body["remaining_ms"].is_null());
}

#[tokio::test]
async fn mode_freezes_after_attempts_but_times_extend_live() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ext_t", "teacher").await;
    let student = login(&app, "elif").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "math").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();
    let ends = now + 60_000;

    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends }),
    )
    .await;

    // Before any attempt the whole schedule is editable — even the mode...
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "async", "duration_ms": 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["mode"], "async");

    // ...but a PATCH may not leave a half-schedule behind.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Clearing everything unschedules; restoring re-schedules.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": null, "starts_at": null, "ends_at": null, "duration_ms": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["mode"].is_null());
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "sync", "starts_at": now - 1_000, "ends_at": ends })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A student sits down; the mode is now frozen (unscheduling included).
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "async", "duration_ms": 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": null, "starts_at": null, "ends_at": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // ...but extending the window moves the running deadline immediately.
    let extended = ends + 300_000;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "ends_at": extended })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["status"], "in_progress");
    assert_eq!(res.body["deadline"].as_i64(), Some(extended));
}

#[tokio::test]
async fn attempts_cascade_with_exam_and_course_deletion() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "casc_t", "teacher").await;
    let student = login(&app, "cansu").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "hist").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();
    let schedule = json!({ "title": "final", "kind": "final", "weight": 1,
                           "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 });

    // Deleting the exam removes its attempts.
    let exam = scheduled_exam(&app, &teacher, &course, schedule.clone()).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(
        ExamAttempt::any_for_exam(&ExamId::from_key(&exam), &db)
            .await
            .unwrap()
    );
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        !ExamAttempt::any_for_exam(&ExamId::from_key(&exam), &db)
            .await
            .unwrap()
    );

    // Deleting the whole course cascades through its exams' attempts too.
    let exam = scheduled_exam(&app, &teacher, &course, schedule).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        !ExamAttempt::any_for_exam(&ExamId::from_key(&exam), &db)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn live_stream_is_sse_and_teacher_scoped() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sse_t", "teacher").await;
    let student = login(&app, "selin").await;
    let course = create_course(&app, &teacher, "geo").await;
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;

    // Teacher: 200 with SSE headers. The body is an endless stream, so only
    // the head of the response is inspected (common::send would hang).
    let request = Request::builder()
        .method("GET")
        .uri(format!("/exams/{exam}/live/stream"))
        .header("cookie", &teacher)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .expect("content-type")
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );
    drop(response);

    // Students can't watch the monitor; a missing exam is a 404 up front.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live/stream"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", "/exams/nope/live/stream", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

// --- exam questions + answers (taking an exam) -----------------------------

/// Create a question via `POST /exams/{exam}/questions` (asserts 201);
/// returns its id.
async fn create_question(
    app: &axum::Router,
    cookie: &str,
    exam: &str,
    body: serde_json::Value,
) -> String {
    let res = send(
        app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(cookie),
        Some(body),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create question: {}",
        res.body
    );
    id_of(&res.body)
}

/// A course with an open sync window and one enrolled student, the spine of
/// the question/answer tests. Returns (course, exam).
async fn open_exam_with_student(
    app: &axum::Router,
    teacher: &str,
    student_id: &str,
    course_title: &str,
) -> (String, String) {
    let course = create_course(app, teacher, course_title).await;
    enroll(app, teacher, &course, student_id).await;
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        app,
        teacher,
        &course,
        json!({ "title": "final", "kind": "final", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    (course, exam)
}

#[tokio::test]
async fn question_crud_validation_and_rbac() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "q_t", "teacher").await;
    let other_teacher = login_as(&app, &db, "q_t2", "teacher").await;
    let student = login(&app, "quinn").await;
    let course = create_course(&app, &teacher, "logic").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final", 1).await;

    // A choice question echoes its full authoring view, correct included.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                     "choices": ["3", "4", "5"], "correct": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["kind"], "choice");
    assert_eq!(res.body["points"], 10);
    assert_eq!(res.body["choices"][1], "4");
    assert_eq!(res.body["correct"], 1);
    let choice_q = id_of(&res.body);

    // A text question carries neither choices nor correct.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "text": "Explain.", "kind": "text", "points": 20 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert!(res.body["choices"].is_null());
    assert!(res.body["correct"].is_null());
    let text_q = id_of(&res.body);

    // The teacher list shows both, in creation order.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let questions = res.body.as_array().expect("questions");
    assert_eq!(questions.len(), 2);
    assert_eq!(id_of(&questions[0]), choice_q);
    assert_eq!(id_of(&questions[1]), text_q);

    // Kind-dependent fields are validated as a unit.
    for (label, body) in [
        (
            "unknown kind",
            json!({ "text": "x", "kind": "essay", "points": 1 }),
        ),
        (
            "zero points",
            json!({ "text": "x", "kind": "text", "points": 0 }),
        ),
        (
            "oversized points",
            json!({ "text": "x", "kind": "text", "points": 101 }),
        ),
        (
            "blank text",
            json!({ "text": "  ", "kind": "text", "points": 1 }),
        ),
        (
            "choice without choices",
            json!({ "text": "x", "kind": "choice", "points": 1, "correct": 0 }),
        ),
        (
            "choice without correct",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": ["a", "b"] }),
        ),
        (
            "correct out of range",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": ["a", "b"], "correct": 2 }),
        ),
        (
            "negative correct",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": ["a", "b"], "correct": -1 }),
        ),
        (
            "one choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": ["a"], "correct": 0 }),
        ),
        (
            "eleven choices",
            json!({ "text": "x", "kind": "choice", "points": 1,
                                   "choices": ["a","b","c","d","e","f","g","h","i","j","k"], "correct": 0 }),
        ),
        (
            "blank choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": ["a", " "], "correct": 0 }),
        ),
        (
            "text with choices",
            json!({ "text": "x", "kind": "text", "points": 1, "choices": ["a", "b"] }),
        ),
        (
            "text with correct",
            json!({ "text": "x", "kind": "text", "points": 1, "correct": 0 }),
        ),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions"),
            Some(&teacher),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    // Authoring needs management rights over the course; reading needs teacher+.
    let question_body = json!({ "text": "x", "kind": "text", "points": 1 });
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&student),
        Some(question_body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&other_teacher),
        Some(question_body),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "not the course creator");
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "correct stays teacher-side"
    );

    // PATCH: keep-by-omission, and the kind bundle re-validates as a unit.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "points": 15 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["points"], 15);
    assert_eq!(res.body["correct"], 1, "kind bundle kept");

    // Switching to text must clear the bundle explicitly...
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "kind": "text" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "half-switch: {}",
        res.body
    );
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "kind": "text", "choices": null, "correct": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["choices"].is_null());

    // ...and back, bringing the bundle along.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "kind": "choice", "choices": ["yes", "no"], "correct": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Editing is management-gated too.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&other_teacher),
        Some(json!({ "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Unknown ids are 404s — including a question under the wrong exam.
    let other_exam = create_exam(&app, &teacher, &course, "quiz", "quiz", 1).await;
    for (method, uri) in [
        ("POST", "/exams/missing/questions".to_string()),
        ("GET", "/exams/missing/questions".to_string()),
        ("PATCH", format!("/exams/{exam}/questions/missing")),
        ("DELETE", format!("/exams/{exam}/questions/missing")),
        ("PATCH", format!("/exams/{other_exam}/questions/{choice_q}")),
        (
            "DELETE",
            format!("/exams/{other_exam}/questions/{choice_q}"),
        ),
    ] {
        let body = match method {
            "POST" => Some(json!({ "text": "x", "kind": "text", "points": 1 })),
            "PATCH" => Some(json!({ "points": 2 })),
            _ => None,
        };
        let res = send(&app, method, &uri, Some(&teacher), body).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }

    // Delete removes from the list; deleting is management-gated.
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{text_q}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{text_q}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn questions_freeze_once_attempts_start() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "frz_t", "teacher").await;
    let student = login(&app, "firat").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam) = open_exam_with_student(&app, &teacher, &student_id, "algo").await;
    let question = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4"], "correct": 1 }),
    )
    .await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Create, edit, and delete are all conflicts now.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "text": "late", "kind": "text", "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        Some(json!({ "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn student_question_view_hides_correct_and_embeds_answers() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sv_t", "teacher").await;
    let student = login(&app, "sona").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam) = open_exam_with_student(&app, &teacher, &student_id, "art").await;
    let choice_q = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4"], "correct": 1 }),
    )
    .await;
    create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;

    // No early peek: the student view requires an attempt.
    let uri = format!("/exams/{exam}/attempt/questions");
    let res = send(&app, "GET", &uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "attempt first");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // The sitting view: same order, no `correct` key anywhere, answers null.
    let res = send(&app, "GET", &uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let questions = res.body.as_array().expect("questions");
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0]["kind"], "choice");
    assert_eq!(questions[0]["choices"][1], "4");
    for question in questions {
        assert!(
            question.as_object().unwrap().get("correct").is_none(),
            "correct must never reach a student: {question}"
        );
        assert!(question["answer"].is_null());
    }

    // A saved answer comes back embedded on the next read.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["question"], choice_q);
    assert_eq!(res.body["selected"], 0);
    let saved_at = res.body["updated_at"].as_i64().expect("updated_at");

    let res = send(&app, "GET", &uri, Some(&student), None).await;
    let questions = res.body.as_array().expect("questions");
    assert_eq!(questions[0]["answer"]["selected"], 0);
    assert_eq!(questions[0]["answer"]["updated_at"], saved_at);
    assert!(questions[1]["answer"].is_null());
}

#[tokio::test]
async fn answer_saves_gate_on_attempt_state_and_kind() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ans_t", "teacher").await;
    let student = login(&app, "arda").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam) = open_exam_with_student(&app, &teacher, &student_id, "phys").await;
    let choice_q = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4", "5"], "correct": 1 }),
    )
    .await;
    let text_q = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;
    let answers_uri = format!("/exams/{exam}/attempt/answers");

    // Saving needs an attempt (404 before start).
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "attempt first");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Upsert: re-answering overwrites the one row and re-stamps its clock.
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let first_saved_at = res.body["updated_at"].as_i64().expect("updated_at");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["selected"], 2);
    assert!(
        res.body["updated_at"].as_i64().expect("updated_at") > first_saved_at,
        "a re-save moves updated_at forward"
    );
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": text_q, "text": "because" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["text"], "because");

    // The attempt view counts distinct questions answered, not saves.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["answered"], 2);
    assert_eq!(res.body["question_count"], 2);

    // Payloads that don't fit the question's kind are 400s.
    for (label, body) in [
        (
            "text on a choice question",
            json!({ "question_id": choice_q, "text": "4" }),
        ),
        (
            "both fields",
            json!({ "question_id": choice_q, "selected": 1, "text": "4" }),
        ),
        (
            "selected out of range",
            json!({ "question_id": choice_q, "selected": 3 }),
        ),
        (
            "negative selected",
            json!({ "question_id": choice_q, "selected": -1 }),
        ),
        ("nothing to save", json!({ "question_id": choice_q })),
        (
            "selected on a text question",
            json!({ "question_id": text_q, "selected": 0 }),
        ),
        (
            "oversized text",
            json!({ "question_id": text_q, "text": "x".repeat(10_001) }),
        ),
    ] {
        let res = send(&app, "POST", &answers_uri, Some(&student), Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    // Unknown questions — and questions of *other* exams — are 404s.
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": "missing", "selected": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let foreign_exam = create_exam(&app, &teacher, &course, "other", "quiz", 1).await;
    let foreign_q = create_question(
        &app,
        &teacher,
        &foreign_exam,
        json!({ "text": "not yours", "kind": "choice", "points": 1,
                "choices": ["a", "b"], "correct": 0 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": foreign_q, "selected": 0 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "a real question under the wrong exam: {}",
        res.body
    );

    // Submitting closes the write path.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": 1 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "after submit: {}",
        res.body
    );

    // ...but the sitting view stays readable for review, answers intact.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "review after submit: {}",
        res.body
    );
    assert_eq!(res.body[0]["answer"]["selected"], 2);
    assert_eq!(res.body[1]["answer"]["text"], "because");
}

#[tokio::test]
async fn answer_saves_stop_at_the_deadline() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "dl_t", "teacher").await;
    let student = login(&app, "dila").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "geo").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // A window that closes right after the start — real time, the server
    // judges expiry by `Timestamp::now()`.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "blitz", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 1_500 }),
    )
    .await;
    let question = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "quick", "kind": "choice", "points": 1,
                "choices": ["a", "b"], "correct": 0 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": 0 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "inside the window: {}",
        res.body
    );

    tokio::time::sleep(std::time::Duration::from_millis(1_700)).await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "expired: {}", res.body);

    // The pre-deadline answer survives untouched for grading.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["answers"][0]["selected"], 0);
}

#[tokio::test]
async fn teacher_answer_sheet_judges_choices_and_suggests_a_score() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sc_t", "teacher").await;
    let student = login(&app, "sena").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam) = open_exam_with_student(&app, &teacher, &student_id, "chem").await;
    let q1 = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4"], "correct": 1 }),
    )
    .await;
    let q2 = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "3 + 3?", "kind": "choice", "points": 20,
                "choices": ["6", "7"], "correct": 0 }),
    )
    .await;
    let q3 = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "Explain.", "kind": "text", "points": 30 }),
    )
    .await;
    let sheet_uri = format!("/exams/{exam}/attempts/{student_id}/answers");

    // No attempt yet: no sheet to read.
    let res = send(&app, "GET", &sheet_uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "no attempt yet");

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    for body in [
        json!({ "question_id": q1, "selected": 1 }), // right
        json!({ "question_id": q2, "selected": 1 }), // wrong
        json!({ "question_id": q3, "text": "entropy grows" }),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(&student),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }

    // The grader's view: judged rows plus the suggested score.
    let res = send(&app, "GET", &sheet_uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["user"]["username"], "sena");
    let answers = res.body["answers"].as_array().expect("answers");
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0]["question"], q1);
    assert_eq!(answers[0]["is_correct"], true);
    assert_eq!(answers[1]["is_correct"], false);
    assert_eq!(answers[2]["text"], "entropy grows");
    assert!(
        answers[2]["is_correct"].is_null(),
        "text is the grader's call"
    );
    assert_eq!(res.body["auto_score"]["earned"], 10);
    assert_eq!(
        res.body["auto_score"]["possible"], 30,
        "text points never counted"
    );

    // The sheet is teacher-side only; students keep using their own view.
    let res = send(&app, "GET", &sheet_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn live_monitor_tracks_answer_progress() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "prg_t", "teacher").await;
    let ayla = login(&app, "ayla").await;
    let bora = login(&app, "bora").await;
    let ayla_id = me_id(&app, &ayla).await;
    let bora_id = me_id(&app, &bora).await;
    let (course, exam) = open_exam_with_student(&app, &teacher, &ayla_id, "stats").await;
    enroll(&app, &teacher, &course, &bora_id).await;
    let question = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4"], "correct": 1 }),
    )
    .await;
    create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;

    // Starting reports zero progress out of two questions.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&ayla),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["answered"], 0);
    assert_eq!(res.body["question_count"], 2);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&ayla),
        Some(json!({ "question_id": question, "selected": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let saved_at = res.body["updated_at"].as_i64().expect("updated_at");

    // The monitor: per-student progress and last activity, plus the total.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["question_count"], 2);
    let students = res.body["students"].as_array().expect("students");
    assert_eq!(students[0]["user"]["username"], "ayla");
    assert_eq!(students[0]["answered"], 1);
    assert_eq!(students[0]["last_activity"].as_i64(), Some(saved_at));
    assert_eq!(students[1]["user"]["username"], "bora");
    assert_eq!(students[1]["answered"], 0);
    assert!(students[1]["last_activity"].is_null());
}

#[tokio::test]
async fn questions_and_answers_cascade_with_deletes() {
    use hezarfen_backend::domain::exam_answer::ExamAnswer;
    use hezarfen_backend::domain::exam_question::ExamQuestion;

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "qc_t", "teacher").await;
    let student = login(&app, "cem").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam) = open_exam_with_student(&app, &teacher, &student_id, "geo2").await;
    let q1 = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4"], "correct": 1 }),
    )
    .await;
    let q2 = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    for body in [
        json!({ "question_id": q1, "selected": 1 }),
        json!({ "question_id": q2, "text": "so" }),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(&student),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let exam_id = ExamId::from_key(&exam);
    assert_eq!(
        ExamQuestion::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        ExamAnswer::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .len(),
        2
    );

    // Deleting one question takes its answers with it — the API freezes
    // question deletes once attempts exist, so exercise the domain cascade
    // directly (it also runs under the exam/course cascades below).
    let question = ExamQuestion::list_for_exam(&exam_id, &db)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    question.delete(&db).await.unwrap();
    assert_eq!(
        ExamQuestion::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ExamAnswer::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .len(),
        1,
        "only the deleted question's answer goes"
    );

    // Deleting the exam clears the rest.
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        ExamQuestion::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        ExamAnswer::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .is_empty()
    );

    // And a course delete cascades through its exams' questions and answers.
    let now = Timestamp::now().as_millis();
    let exam2 = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "retake", "kind": "quiz", "weight": 1,
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let q = create_question(
        &app,
        &teacher,
        &exam2,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": ["3", "4"], "correct": 1 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": q, "selected": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let exam2_id = ExamId::from_key(&exam2);
    assert!(
        ExamQuestion::list_for_exam(&exam2_id, &db)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        ExamAnswer::list_for_exam(&exam2_id, &db)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn question_patch_revalidates_the_stale_kind_bundle() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rv_t", "teacher").await;
    let course = create_course(&app, &teacher, "sets").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final", 1).await;
    let question = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": ["a", "b", "c"], "correct": 2 }),
    )
    .await;
    let uri = format!("/exams/{exam}/questions/{question}");

    // Shrinking the options while keeping the old `correct` would leave it
    // dangling past the end — the merge must re-validate the bundle.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "choices": ["a", "b"] })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "kept correct=2 dangles: {}",
        res.body
    );

    // Same shrink with `correct` brought along is fine.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "choices": ["a", "b"], "correct": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["correct"], 1);

    // A lone out-of-range `correct` against the kept choices is caught too.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "correct": 5 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Clearing just one half of the bundle can't leave a half-question.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "correct": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Nothing of the failed PATCHes stuck.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body[0]["choices"].as_array().unwrap().len(), 2);
    assert_eq!(res.body[0]["correct"], 1);
}

#[tokio::test]
async fn question_authoring_follows_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "own_t", "teacher").await;
    let boss = login_as(&app, &db, "own_m", "manager").await;
    let course = create_course(&app, &teacher, "greek").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final", 1).await;

    // A manager+ authors questions in anyone's course, like every other
    // course-management write.
    let question = create_question(
        &app,
        &boss,
        &exam,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": ["a", "b"], "correct": 0 }),
    )
    .await;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&boss),
        Some(json!({ "points": 20 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn concurrent_answer_saves_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "cc_t", "teacher").await;
    let student = login(&app, "cana").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam) = open_exam_with_student(&app, &teacher, &student_id, "race").await;
    let question = create_question(
        &app,
        &teacher,
        &exam,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": ["a", "b", "c"], "correct": 0 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // A burst of autosaves for the same question (fast clicking / reconnect
    // races): the composite-id upsert must converge, never 500.
    let mut handles = Vec::new();
    for i in 0..24i64 {
        let app = app.clone();
        let student = student.clone();
        let uri = format!("/exams/{exam}/attempt/answers");
        let body = json!({ "question_id": question, "selected": i % 3 });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&student), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent save must not fail"
        );
    }

    // One row survives, whichever save won.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["answers"].as_array().unwrap().len(), 1);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["answered"], 1, "24 saves, one answer");
}

// --- course sessions + roll call ------------------------------------------

#[tokio::test]
async fn session_crud_follows_course_management() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let student = login(&app, "ali").await;
    let course = create_course(&app, &owner, "algebra").await;
    let now = Timestamp::now().as_millis();
    let start = now + 3_600_000;

    // Create: student 403, unrelated teacher 403, owner 201, manager+ 201.
    let body = json!({ "topic": "limits", "starts_at": start });
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&student),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&rival),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let session = id_of(&res.body);
    // The teacher defaults to the caller, rendered as a PersonRef.
    assert_eq!(res.body["teacher"]["username"], "owner");
    assert_eq!(res.body["topic"], "limits");
    assert_eq!(res.body["starts_at"], start);
    assert!(res.body["ends_at"].is_null());
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&boss),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "manager+ manages any course"
    );

    // Explicit teacher_id: must exist and be teacher+.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 60_000, "teacher_id": "01UNKNOWN" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "unknown teacher");
    let ali_id = me_id(&app, &student).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 60_000, "teacher_id": ali_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "a student cannot teach"
    );
    let rival_id = me_id(&app, &rival).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 120_000, "teacher_id": rival_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["teacher"]["username"], "rival");
    let rival_session = id_of(&res.body);

    // Time range validated on create and update.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 10_000, "ends_at": now + 5_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Past lessons cannot be scheduled — on create or by a later PATCH,
    // whichever end carries the past value.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 3_600_000, "ends_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "past ends_at");

    // Reads are open to any logged-in user; parents must exist.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/sessions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // owner's, the manager's, and the one taught by `rival` — newest starts_at
    // first, so the two `start` lessons precede the earlier `rival` one.
    assert_eq!(res.body.as_array().unwrap().len(), 3);
    assert_eq!(res.body[2]["starts_at"], now + 120_000);
    let res = send(&app, "GET", "/courses/nope/sessions", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(&app, "GET", "/sessions/nope", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // PATCH: management rights; set/keep/clear semantics; range check.
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&rival),
        Some(json!({ "topic": "hijack" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "topic": "derivatives", "ends_at": start + 100_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["topic"], "derivatives");
    assert_eq!(res.body["ends_at"], start + 100_000);
    assert_eq!(res.body["starts_at"], start, "omitted keeps");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "ends_at": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["ends_at"].is_null(), "explicit null clears");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "ends_at": now + 1_800_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "ends before starts");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "starts_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "backdated start");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "ends_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "backdated end");
    // A lesson already underway (start inside the grace) stays editable as
    // long as the past time is only kept, not re-sent.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "topic": "underway", "starts_at": now - 1_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let underway = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{underway}"),
        Some(&owner),
        Some(json!({ "topic": "still editable" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // Reassigning the teacher revalidates the target.
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{rival_session}"),
        Some(&boss),
        Some(json!({ "teacher_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // DELETE: management rights.
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{session}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{session}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn roll_call_rbac_and_upsert() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;
    let hoca_id = me_id(&app, &hoca).await;

    let course = create_course(&app, &owner, "algebra").await;
    enroll(&app, &owner, &course, &ali_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(
            json!({ "starts_at": Timestamp::now().as_millis() + 3_600_000, "teacher_id": hoca_id }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let session = id_of(&res.body);
    let mark_uri = format!("/sessions/{session}/attendance");

    // The session's teacher takes roll even without course-management rights.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["user"]["username"], "ali");
    assert_eq!(res.body["marked_by"]["username"], "hoca");
    assert_eq!(res.body["course"].as_str().unwrap(), course);

    // Re-marking overwrites — one row per session+user (course manager may too).
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&owner),
        Some(json!({ "status": "absent", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(&app, "GET", &mark_uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK, "roster is readable by students");
    let roster = res.body.as_array().unwrap();
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["status"], "absent");

    // Neither an unrelated teacher nor the student themselves may mark.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&rival),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&ali),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "students never self-mark"
    );

    // The roster comes from enrollment; targets must exist; statuses are closed.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "not enrolled");
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": "01UNKNOWN" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "sleeping", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        "/sessions/nope/attendance",
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // The session teacher's own row is management's call: even the course
    // creator (teacher role) can't write it; a manager can.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": hoca_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "no self-declared presence"
    );
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&owner),
        Some(json!({ "status": "present", "user_id": hoca_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&boss),
        Some(json!({ "status": "late", "user_id": hoca_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Removal mirrors marking rights.
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{hoca_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "teacher row needs manager+"
    );
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{hoca_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "row already gone");
}

#[tokio::test]
async fn deleting_session_or_course_cascades_roll_call() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let course = create_course(&app, &owner, "algebra").await;
    enroll(&app, &owner, &course, &ali_id).await;
    let now = Timestamp::now().as_millis();
    let s1 = create_session(&app, &owner, &course, now + 3_600_000).await;
    let s2 = create_session(&app, &owner, &course, now + 7_200_000).await;
    for s in [&s1, &s2] {
        let res = send(
            &app,
            "POST",
            &format!("/sessions/{s}/attendance"),
            Some(&owner),
            Some(json!({ "status": "present", "user_id": ali_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }
    let report = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(report.body["sessions"]["total"], 2);

    // Deleting one session removes exactly its rows...
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{s1}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let report = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(report.body["sessions"]["total"], 1);

    // ...and deleting the course removes the rest, sessions included.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &format!("/sessions/{s2}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let report = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(report.body["sessions"]["total"], 0);
    assert!(report.body["courses"].as_array().unwrap().is_empty());
}

// --- work log ---------------------------------------------------------------

#[tokio::test]
async fn work_log_lifecycle_is_server_stamped_and_exclusive() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali = login(&app, "ali").await;

    // Staff only.
    let res = send(&app, "POST", "/work/check-in", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", "/work/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Check out before check-in conflicts.
    let res = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["check_out"].is_null());
    let first_in = res.body["check_in"].as_i64().unwrap();

    // A second check-in neither opens a second stint nor resets the clock.
    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = send(&app, "GET", "/work/me", Some(&hoca), None).await;
    let log = res.body.as_array().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["check_in"].as_i64().unwrap(), first_in);

    let res = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let out = res.body["check_out"].as_i64().unwrap();
    assert!(out >= first_in);
    assert_eq!(res.body["duration_ms"].as_i64().unwrap(), out - first_in);
    let res = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "already checked out");

    // The open slot is free again; the log keeps both stints, newest first.
    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(&app, "GET", "/work/me", Some(&hoca), None).await;
    let log = res.body.as_array().unwrap();
    assert_eq!(log.len(), 2);
    assert!(log[0]["check_out"].is_null(), "open stint sorts newest");
    assert!(!log[1]["check_out"].is_null());
}

#[tokio::test]
async fn work_log_manager_reads_and_corrections() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let other = login_as(&app, &db, "other", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let hoca_id = me_id(&app, &hoca).await;

    // One closed stint and one open stint for hoca.
    send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    let closed = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    let closed_id = id_of(&closed.body);
    let open = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    let open_id = id_of(&open.body);

    // Reading someone's log is manager+; unknown users are 404.
    let res = send(&app, "GET", &format!("/work/{hoca_id}"), Some(&other), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", &format!("/work/{hoca_id}"), Some(&boss), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body.as_array().unwrap().len(), 2);
    let res = send(&app, "GET", "/work/01UNKNOWN", Some(&boss), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Corrections: manager+ only, closed entries only, ordered instants only.
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&hoca),
        Some(json!({ "check_in": 1000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&boss),
        Some(json!({ "check_in": 1000, "check_out": 2000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["check_in"], 1000);
    assert_eq!(res.body["check_out"], 2000);
    assert_eq!(res.body["duration_ms"], 1000);
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&boss),
        Some(json!({ "check_out": 500 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "out precedes in");
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{open_id}"),
        Some(&boss),
        Some(json!({ "check_in": 1 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "open entries aren't correctable"
    );
    let res = send(
        &app,
        "PATCH",
        "/work/entries/nope",
        Some(&boss),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Deletion: manager+, any entry (open ones included — the stray-row broom).
    let res = send(
        &app,
        "DELETE",
        &format!("/work/entries/{closed_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/work/entries/{open_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/work/entries/{open_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    // With the open row swept, hoca can check in again.
    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
}

// --- attendance reports -------------------------------------------------------

#[tokio::test]
async fn attendance_report_tallies_events_and_sessions_per_course() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;

    // A fresh user's report is all zeros with null rates.
    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["events"]["total"], 0);
    assert!(res.body["events"]["rate"].is_null());
    assert!(res.body["courses"].as_array().unwrap().is_empty());

    // Two event rows: one self-marked, one teacher-marked.
    let ev1 = send(
        &app,
        "POST",
        "/events",
        Some(&owner),
        Some(json!({ "title": "assembly" })),
    )
    .await;
    let ev2 = send(
        &app,
        "POST",
        "/events",
        Some(&owner),
        Some(json!({ "title": "trip" })),
    )
    .await;
    let (ev1, ev2) = (id_of(&ev1.body), id_of(&ev2.body));
    send(
        &app,
        "POST",
        &format!("/events/{ev1}/attendance"),
        Some(&ali),
        Some(json!({ "status": "present" })),
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/events/{ev2}/attendance"),
        Some(&owner),
        Some(json!({ "status": "absent", "user_id": ali_id })),
    )
    .await;

    // Roll call across two courses: algebra present+late, physics absent+excused.
    let algebra = create_course(&app, &owner, "algebra").await;
    let physics = create_course(&app, &owner, "physics").await;
    for (course, statuses) in [
        (&algebra, ["present", "late"]),
        (&physics, ["absent", "excused"]),
    ] {
        enroll(&app, &owner, course, &ali_id).await;
        for (n, status) in statuses.iter().enumerate() {
            let session = create_session(
                &app,
                &owner,
                course,
                Timestamp::now().as_millis() + (n as i64 + 1) * 3_600_000,
            )
            .await;
            let res = send(
                &app,
                "POST",
                &format!("/sessions/{session}/attendance"),
                Some(&owner),
                Some(json!({ "status": status, "user_id": ali_id })),
            )
            .await;
            assert_eq!(res.status, StatusCode::OK);
        }
    }

    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["user"].as_str().unwrap(), ali_id);
    // Events: 1 present + 1 absent → rate 1/2.
    assert_eq!(res.body["events"]["present"], 1);
    assert_eq!(res.body["events"]["absent"], 1);
    assert_eq!(res.body["events"]["total"], 2);
    assert_eq!(res.body["events"]["rate"].as_f64().unwrap(), 0.5);
    // Sessions overall: present+late+absent+excused → rate (1+1)/3.
    assert_eq!(res.body["sessions"]["total"], 4);
    assert_eq!(res.body["sessions"]["excused"], 1);
    let rate = res.body["sessions"]["rate"].as_f64().unwrap();
    assert!(
        (rate - 2.0 / 3.0).abs() < 1e-12,
        "excused stays out: {rate}"
    );
    // Per-course blocks: algebra 100% attended, physics 0% (excused uncounted).
    let courses = res.body["courses"].as_array().unwrap();
    assert_eq!(courses.len(), 2);
    let block = |title: &str| {
        courses
            .iter()
            .find(|b| b["course"]["title"] == title)
            .unwrap_or_else(|| panic!("missing course block {title}"))
            .clone()
    };
    assert_eq!(block("algebra")["counts"]["rate"].as_f64().unwrap(), 1.0);
    assert_eq!(block("algebra")["counts"]["late"], 1);
    assert_eq!(block("physics")["counts"]["rate"].as_f64().unwrap(), 0.0);
    assert_eq!(block("physics")["counts"]["excused"], 1);

    // Unenrolling hides marks, never absences: the physics block stays.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{physics}/enrollments/{ali_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.body["courses"].as_array().unwrap().len(), 2);

    // Someone else's report: teacher+ only; unknown users are 404.
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["sessions"]["total"], 4);
    let res = send(&app, "GET", "/attendance/01UNKNOWN", Some(&owner), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}
