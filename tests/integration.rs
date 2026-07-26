//! Integration tests: drive the assembled axum router directly (no network)
//! against a fresh in-memory SurrealDB, via `tower::ServiceExt::oneshot`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    app_and_db, create_course, create_exam, create_exam_with, create_homework, create_session,
    create_subject, enroll, id_of, login, login_as, me_id, mem_app, send, set_role, unenroll,
};
use hezarfen_backend::constant::BANK_VISIBILITY_SCHOOL;
use hezarfen_backend::domain::chatbot_message::ChatbotMessage;
use hezarfen_backend::domain::chatbot_thread::ChatbotThreadId;
use hezarfen_backend::domain::exam::ExamId;
use hezarfen_backend::domain::exam_attempt::ExamAttempt;
use hezarfen_backend::domain::session::Session;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::{Password, User, UserId, Username};
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
async fn limits_publishes_the_bounds_the_api_actually_enforces() {
    let app = mem_app().await;

    let res = send(&app, "GET", "/limits", None, None).await;
    assert_eq!(res.status, StatusCode::OK);

    // Every group is present — a frontend reads one document, not thirteen.
    for group in [
        "user",
        "note",
        "file",
        "message",
        "event",
        "course",
        "exam",
        "homework",
        "question_pool",
        "appointment",
        "chatbot",
        "settings",
        "request",
    ] {
        assert!(res.body[group].is_object(), "missing group: {group}");
    }

    // The published value must equal the one the constants carry — the whole
    // point is that the frontend stops keeping its own copy.
    assert_eq!(
        res.body["user"]["max_username_len"],
        json!(hezarfen_backend::constant::MAX_USERNAME_LEN)
    );
    assert_eq!(res.body["exam"]["modes"], json!(["sync", "async", "open"]));
    assert_eq!(
        res.body["user"]["roles"],
        json!(["parent", "student", "teacher", "manager", "admin"])
    );

    // The request budgets a caller is actually held to, read live off this
    // server rather than from a constant — a client should learn its budget
    // here, not by collecting a 429.
    assert!(res.body["rate"]["auth_per_minute"].is_number());
    assert!(res.body["rate"]["api_per_minute"].is_number());
    assert!(res.body["rate"]["chatbot_per_minute"].is_number());
    assert_eq!(res.body["rate"]["window_secs"], json!(60));

    // Structural sanity over EVERY published pair, which is what a spot-check
    // of a few named fields cannot give: any `min_x` must not exceed its
    // `max_x`. This is what catches a mapping that wired a floor to a ceiling
    // — the one mistake a "does the constant appear?" check cannot see.
    let groups = res.body.as_object().expect("limits is an object");
    let mut checked = 0;
    for (group, fields) in groups {
        let fields = fields.as_object().expect("each group is an object");
        for (key, value) in fields {
            let Some(bare) = key.strip_prefix("min_") else {
                continue;
            };
            let Some(max) = fields.get(&format!("max_{bare}")) else {
                continue;
            };
            let (min, max) = (
                value.as_i64().expect("min is an integer"),
                max.as_i64().expect("max is an integer"),
            );
            assert!(
                min <= max,
                "{group}.{key} ({min}) exceeds its maximum ({max}) — the pair is inverted"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 10,
        "only {checked} min/max pairs were checked — the pairing broke, not the values"
    );

    // …and the endpoint that enforces it must agree: one over the published
    // maximum is refused. This is what catches a constant moving without the
    // limits response, or the reverse.
    let max = res.body["user"]["max_username_len"]
        .as_u64()
        .expect("max_username_len is an integer") as usize;
    let too_long = json!({ "username": "a".repeat(max + 1), "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(too_long)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn limits_still_answers_while_the_database_is_down() {
    // A frontend booting against a backend whose database is reconnecting is
    // precisely when it needs the validation contract. `/limits` touches no
    // row, so the outage guard must let it through — otherwise the client
    // falls back to the hard-coded copy this endpoint exists to remove.
    let db_up = hezarfen_backend::state::DbHealth::default();
    let app = build_router(AppState {
        db: database::init_mem().await.expect("in-memory db"),
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        db_up: db_up.clone(),
        ai: None,
    });
    db_up.set(false);

    let res = send(&app, "GET", "/limits", None, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["user"]["max_username_len"].is_number());

    // Everything that does touch the database still refuses, so the exemption
    // is scoped to the one static route and has not disarmed the guard.
    let res = send(&app, "GET", "/settings", None, None).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
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

    // Uppercase anywhere in the username -> 400.
    for bad in ["Bob", "bOb", "BOB"] {
        let upper = json!({ "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(upper)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad} accepted");
    }

    // Username must start and end with a letter or digit -> 400.
    for bad in ["-bob", "bob-", "_bob", "bob_", ".bob"] {
        let edge = json!({ "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(edge)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad} accepted");
    }

    // Interior characters are an allowlist: lowercase, digits, . _ - only.
    for bad in [
        "a b",
        "a\nb",
        "a\u{1b}[31mb",
        "a<b>c",
        "a@b.c",
        "a/b",
        "a\"b",
    ] {
        let ugly = json!({ "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(ugly)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad:?} accepted");
    }

    // Consecutive separators -> 400.
    for bad in ["a--b", "a..b", "a__b", "a.-b"] {
        let doubled = json!({ "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(doubled)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad} accepted");
    }

    // Single separators between alphanumerics are fine -> 201.
    let dotted = json!({ "username": "ali.k_1-b", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(dotted)).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "ali.k_1-b");

    // Valid -> 201, no password echoed back, defaults to the student role.
    let ok = json!({ "username": "bob", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(ok.clone())).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "bob");
    assert_eq!(res.body["role"], "student");
    assert!(res.body.get("password_hash").is_none());

    // A duplicate answers 201 like any other register (the "taken" reply is
    // deliberately indistinguishable — see tests/auth_enumeration.rs), but the
    // write is still rejected: the original password keeps working and the
    // second one never becomes valid.
    let dup = json!({ "username": "bob", "password": "hijack1" });
    let res = send(&app, "POST", "/auth/register", None, Some(dup.clone())).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(&app, "POST", "/auth/login", None, Some(ok)).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "original password stopped working"
    );
    let res = send(&app, "POST", "/auth/login", None, Some(dup)).await;
    assert_eq!(
        res.status,
        StatusCode::UNAUTHORIZED,
        "the duplicate register overwrote the account"
    );
}

#[tokio::test]
async fn register_rejects_reserved_usernames() {
    let app = mem_app().await;

    // Staff-looking names can't be claimed through public registration.
    for name in [
        "admin",
        "administrator",
        "root",
        "support",
        "system",
        "moderator",
        "staff",
    ] {
        let creds = json!({ "username": name, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{name} accepted");
        let msg = res.body["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("reserved"),
            "unexpected error for {name}: {msg}"
        );
    }

    // The reservation is registration-only policy: the seeded bootstrap admin
    // (created through `ensure_admin`, not `/auth/register`) still logs in.
    // Covered by the admin bootstrap tests; here just prove a reserved name
    // is not permanently poisoned for login by the register-level check.
    let creds = json!({ "username": "admin", "password": "wrong" });
    let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED); // bad credentials, not "reserved"
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
        ("POST", "/events/x/attendance"),
        ("GET", "/events/x/roster"),
        ("GET", "/exams"),
        ("GET", "/courses"),
        ("POST", "/courses"),
        ("GET", "/courses/me"),
        ("GET", "/marks/me"),
        ("POST", "/exams/x/attempt"),
        ("GET", "/exams/x/attempt"),
        ("POST", "/exams/x/attempt/finish"),
        ("GET", "/exams/x/live"),
        ("POST", "/exams/x/questions"),
        ("GET", "/exams/x/questions"),
        ("PATCH", "/exams/x/questions/y"),
        ("DELETE", "/exams/x/questions/y"),
        ("GET", "/exams/x/attempt/questions"),
        ("POST", "/exams/x/attempt/answers"),
        ("GET", "/exams/x/attempts/u/answers"),
        ("POST", "/exams/x/questions/y/image"),
        ("GET", "/exams/x/questions/y/image"),
        ("DELETE", "/exams/x/questions/y/image"),
        ("POST", "/exams/x/questions/y/choices/0/image"),
        ("GET", "/exams/x/questions/y/choices/0/image"),
        ("DELETE", "/exams/x/questions/y/choices/0/image"),
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
        ("POST", "/pomodoro/start"),
        ("POST", "/pomodoro/finish"),
        ("GET", "/pomodoro/me"),
        ("GET", "/pomodoro/u"),
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
        common::items(&send(&app, "GET", "/notes", Some(&ali), None).await.body).len(),
        1
    );
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&veli), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/notes", Some(&veli), None).await.body).len(),
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

/// Regression: two partial PATCHes touching *different* fields must both
/// survive. The handler used to fill the field a request omitted from the
/// snapshot it had read, so the writer that landed second reverted the other
/// one's field — a field-scoped `SET` does not help while the value bound to
/// it comes from a stale read. Raced repeatedly: a single round only loses the
/// update on the interleaving where both handlers read before either writes.
#[tokio::test]
async fn concurrent_partial_note_patches_keep_both_fields() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;

    for round in 0..20 {
        let note = create_note(&app, &ali, "old title").await;
        let uri = format!("/notes/{note}");
        let title_patch = json!({ "title": "new title" });
        let content_patch = json!({ "content": "new body" });
        let (a, b) = tokio::join!(
            send(&app, "PATCH", &uri, Some(&ali), Some(title_patch)),
            send(&app, "PATCH", &uri, Some(&ali), Some(content_patch)),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} title patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} content patch");

        let after = send(&app, "GET", &uri, Some(&ali), None).await.body;
        assert_eq!(after["title"], "new title", "round {round}: title reverted");
        assert_eq!(
            after["content"], "new body",
            "round {round}: content reverted"
        );
    }
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

// --- note files ------------------------------------------------------------

/// Create a note as `cookie`; returns its id.
async fn create_note(app: &axum::Router, cookie: &str, title: &str) -> String {
    let res = send(
        app,
        "POST",
        "/notes",
        Some(cookie),
        Some(json!({ "title": title })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create note {title}");
    id_of(&res.body)
}

#[tokio::test]
async fn note_files_upload_download_delete_roundtrip() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "with file").await;

    let bytes = b"%PDF-1.4 fake but binary \x00\x01\x02".to_vec();
    let up = common::upload_file(&app, &ali, &note, "plan.pdf", "application/pdf", &bytes).await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(up.body["name"], "plan.pdf");
    assert_eq!(up.body["content_type"], "application/pdf");
    assert_eq!(up.body["size"], bytes.len() as i64);
    let file_id = id_of(&up.body);

    // Listed under the note (paged envelope).
    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(common::total(&list.body), 1);
    assert_eq!(common::items(&list.body)[0]["name"], "plan.pdf");

    // Download returns the exact bytes with the declared type and filename.
    let (status, headers, body) = common::send_raw(
        &app,
        "GET",
        &format!("/notes/{note}/files/{file_id}"),
        Some(&ali),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"plan.pdf\"; filename*=UTF-8''plan.pdf"
    );

    // Delete; the file is gone from the list, download 404s, blob removed.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note}/files/{file_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&list.body), 0);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/notes/{note}/files/{file_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        !common::files_dir().join(&file_id).exists(),
        "blob must be unlinked with its row"
    );
}

#[tokio::test]
async fn note_files_are_scoped_to_the_notes_owner() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let note = create_note(&app, &ali, "mine").await;
    let up = common::upload_file(&app, &ali, &note, "a.txt", "text/plain", b"hi").await;
    assert_eq!(up.status, StatusCode::CREATED);
    let file_id = id_of(&up.body);

    // Another user can't upload to, list, download from, or delete on the note.
    let foreign = common::upload_file(&app, &veli, &note, "b.txt", "text/plain", b"x").await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/notes/{note}/files"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    for method in ["GET", "DELETE"] {
        assert_eq!(
            send(
                &app,
                method,
                &format!("/notes/{note}/files/{file_id}"),
                Some(&veli),
                None
            )
            .await
            .status,
            StatusCode::NOT_FOUND,
            "{method}"
        );
    }

    // A file id under someone else's note id doesn't resolve either.
    let veli_note = create_note(&app, &veli, "veli's").await;
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/notes/{veli_note}/files/{file_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn note_file_size_follows_the_school_limit() {
    let (app, db) = app_and_db().await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "sized").await;

    // Lower the school cap to 1 KiB.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "max_file_bytes": 1024 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["max_file_bytes"], 1024);

    // Over the cap dies with 413; at the cap passes.
    let big = common::upload_file(&app, &ali, &note, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", big.body);
    let fits = common::upload_file(&app, &ali, &note, "fits.bin", "", &vec![7u8; 1024]).await;
    assert_eq!(fits.status, StatusCode::CREATED, "{}", fits.body);
    // A blank part content type falls back to octet-stream.
    assert_eq!(fits.body["content_type"], "application/octet-stream");

    // Raising the cap unlocks bigger files at once.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/settings",
            Some(&boss),
            Some(json!({ "max_file_bytes": 10_000 })),
        )
        .await
        .status,
        StatusCode::OK
    );
    let now_fits = common::upload_file(&app, &ali, &note, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(now_fits.status, StatusCode::CREATED);
}

#[tokio::test]
async fn settings_bound_the_file_limit() {
    let (app, db) = app_and_db().await;
    let boss = login_as(&app, &db, "boss", "manager").await;

    // The default is visible to any authenticated user.
    let res = send(&app, "GET", "/settings", Some(&boss), None).await;
    assert_eq!(res.body["max_file_bytes"], 5 * 1024 * 1024);

    // Outside the hard bounds -> 400 (and the stored value is untouched).
    for bad in [0, 1023, 25 * 1024 * 1024 + 1] {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(&boss),
            Some(json!({ "max_file_bytes": bad })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "cap {bad}");
    }
    let res = send(&app, "GET", "/settings", Some(&boss), None).await;
    assert_eq!(res.body["max_file_bytes"], 5 * 1024 * 1024);

    // Editing another field leaves the cap alone.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "max_file_bytes": 2048 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "grade_bands": [{ "min": 0, "label": "F" }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["max_file_bytes"], 2048);
}

#[tokio::test]
async fn note_file_count_is_capped() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "full").await;

    for i in 0..10 {
        let up = common::upload_file(&app, &ali, &note, &format!("f{i}.txt"), "", b"x").await;
        assert_eq!(up.status, StatusCode::CREATED, "file {i}");
    }
    let over = common::upload_file(&app, &ali, &note, "f10.txt", "", b"x").await;
    assert_eq!(over.status, StatusCode::CONFLICT);

    // Deleting one frees a slot.
    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    let victim = id_of(&common::items(&list.body)[0]);
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note}/files/{victim}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let retry = common::upload_file(&app, &ali, &note, "f10.txt", "", b"x").await;
    assert_eq!(retry.status, StatusCode::CREATED);
}

/// Regression: the 10-file cap was a bare count-then-write — concurrent
/// uploads all read the same pre-count and pushed a note past the cap (the
/// same write-skew as the event-capacity over-admit; SurrealDB transactions
/// don't conflict-check a cross-record count against an insert). The insert
/// now recounts under a lock, so the outcome is race-order independent:
/// exactly enough uploads win to land on the cap, the rest get the same 409
/// as a sequential over-fill.
#[tokio::test]
async fn concurrent_uploads_never_exceed_the_file_cap() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "contested").await;

    for i in 0..8 {
        let up = common::upload_file(&app, &ali, &note, &format!("f{i}.txt"), "", b"x").await;
        assert_eq!(up.status, StatusCode::CREATED, "seed file {i}");
    }

    // Four racers for the two remaining slots.
    let results = tokio::join!(
        common::upload_file(&app, &ali, &note, "r0.txt", "", b"x"),
        common::upload_file(&app, &ali, &note, "r1.txt", "", b"x"),
        common::upload_file(&app, &ali, &note, "r2.txt", "", b"x"),
        common::upload_file(&app, &ali, &note, "r3.txt", "", b"x"),
    );
    let statuses = [
        results.0.status,
        results.1.status,
        results.2.status,
        results.3.status,
    ];
    for status in statuses {
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "a lost cap race must be a 409, got {status}"
        );
    }

    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        common::total(&list.body),
        10,
        "the cap must hold exactly under concurrency, statuses={statuses:?}"
    );
}

#[tokio::test]
async fn note_file_upload_validation() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "strict").await;

    // Empty file, filename with a path separator, no file field at all: 400.
    let empty = common::upload_file(&app, &ali, &note, "e.txt", "", b"").await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);
    let traversal = common::upload_file(&app, &ali, &note, "../../etc/passwd", "", b"data").await;
    assert_eq!(traversal.status, StatusCode::BAD_REQUEST);

    // A body whose only part isn't named "file" has no upload in it.
    let stray = common::multipart_file("y.txt", "", b"data");
    let stray = String::from_utf8(stray)
        .unwrap()
        .replace("name=\"file\"", "name=\"attachment\"");
    let (status, _, _) = common::send_raw(
        &app,
        "POST",
        &format!("/notes/{note}/files"),
        Some(&ali),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        stray.into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Non-ASCII filenames are fine — stored and encoded on download.
    let turkish =
        common::upload_file(&app, &ali, &note, "ödev 1.pdf", "application/pdf", b"pdf").await;
    assert_eq!(turkish.status, StatusCode::CREATED);
    let (status, headers, _) = common::send_raw(
        &app,
        "GET",
        &format!("/notes/{note}/files/{}", id_of(&turkish.body)),
        Some(&ali),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"_dev 1.pdf\"; filename*=UTF-8''%C3%B6dev%201.pdf"
    );
}

#[tokio::test]
async fn deleting_a_note_removes_its_files() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "doomed").await;
    let up = common::upload_file(&app, &ali, &note, "gone.txt", "", b"bye").await;
    assert_eq!(up.status, StatusCode::CREATED);
    let file_id = id_of(&up.body);

    assert_eq!(
        send(&app, "DELETE", &format!("/notes/{note}"), Some(&ali), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // Row and blob are both gone.
    let mut rows = db
        .query("SELECT * FROM note_file")
        .await
        .unwrap()
        .check()
        .unwrap();
    let left: Vec<serde_json::Value> = rows.take(0).unwrap();
    assert!(left.is_empty(), "note_file rows must cascade: {left:?}");
    assert!(
        !common::files_dir().join(&file_id).exists(),
        "blob must be unlinked when its note dies"
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
        common::items(&send(&app, "GET", "/events", Some(&veli), None).await.body).len(),
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
        common::items(
            &send(
                &app,
                "GET",
                &format!("/events/{event_id}/attendance"),
                Some(&ali),
                None
            )
            .await
            .body
        )
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
        common::items(
            &send(
                &app,
                "GET",
                &format!("/events/{event_id}/attendance"),
                Some(&ali),
                None
            )
            .await
            .body
        )
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

#[tokio::test]
async fn event_audience_defaults_validates_and_echoes() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    // Omitted audience = school-wide, and it echoes on every read.
    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "assembly" })),
    )
    .await;
    assert_eq!(ev.status, StatusCode::CREATED);
    assert_eq!(ev.body["audience"]["kind"], "school");

    // A role audience round-trips…
    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "orientation", "audience": { "kind": "role", "role": "student" } })),
    )
    .await;
    assert_eq!(ev.status, StatusCode::CREATED);
    assert_eq!(ev.body["audience"]["role"], "student");
    let event_id = id_of(&ev.body);

    // …a PATCH without `audience` keeps it, and a sent one replaces it.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "title": "orientation day" })),
    )
    .await;
    assert_eq!(res.body["audience"]["role"], "student");
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "audience": { "kind": "registration", "capacity": 5 } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["audience"]["capacity"], 5);

    // An omitted capacity echoes as null: an uncapped signup list.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "trip", "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["audience"]["kind"], "registration");
    assert!(res.body["audience"]["capacity"].is_null());

    // Invalid audiences are rejected: unknown role, ghost course, and
    // non-positive capacities.
    for audience in [
        json!({ "kind": "role", "role": "wizard" }),
        json!({ "kind": "course", "course": "nope" }),
        json!({ "kind": "registration", "capacity": 0 }),
        json!({ "kind": "registration", "capacity": -3 }),
    ] {
        let res = send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "bad", "audience": audience })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{audience}");
    }
    // An unknown kind never deserializes — including the retired hand-picked
    // `users` kind, gone in the registration clean break.
    for audience in [
        json!({ "kind": "galaxy" }),
        json!({ "kind": "users", "users": ["ghost"] }),
    ] {
        let res = send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "bad", "audience": audience })),
        )
        .await;
        assert!(res.status.is_client_error(), "{audience}: {}", res.status);
    }
}

#[tokio::test]
async fn event_audience_gates_marking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await; // student
    let mina = login(&app, "mina").await; // student
    let veli_id = me_id(&app, &veli).await;
    let mina_id = me_id(&app, &mina).await;
    let ali_id = me_id(&app, &ali).await;

    let create = |audience: serde_json::Value| {
        let app = &app;
        let ali = &ali;
        async move {
            let res = send(
                app,
                "POST",
                "/events",
                Some(ali),
                Some(json!({ "title": "ev", "audience": audience })),
            )
            .await;
            assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
            id_of(&res.body)
        }
    };
    let mark = |event: String, user: String| {
        let app = &app;
        let ali = &ali;
        async move {
            send(
                app,
                "POST",
                &format!("/events/{event}/attendance"),
                Some(ali),
                Some(json!({ "status": "present", "user_id": user })),
            )
            .await
            .status
        }
    };

    // Role audience: students are markable, the (teacher) marker is not —
    // including implicitly, via the marker-defaults-to-caller path.
    let ev = create(json!({ "kind": "role", "role": "student" })).await;
    assert_eq!(mark(ev.clone(), veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        mark(ev.clone(), ali_id.clone()).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{ev}/attendance"),
            Some(&ali),
            Some(json!({ "status": "present" })),
        )
        .await
        .status,
        StatusCode::BAD_REQUEST,
        "caller-defaulted target must still clear the audience gate"
    );

    // Course audience: enrollment decides, live.
    let course = create_course(&app, &ali, "algebra").await;
    enroll(&app, &ali, &course, &veli_id).await;
    let ev = create(json!({ "kind": "course", "course": course })).await;
    assert_eq!(mark(ev.clone(), veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        mark(ev.clone(), mina_id.clone()).await,
        StatusCode::BAD_REQUEST
    );

    // Registration audience: the signup list is the roster.
    let ev = create(json!({ "kind": "registration" })).await;
    let res = send(
        &app,
        "POST",
        &format!("/events/{ev}/register"),
        Some(&ali),
        Some(json!({ "user_id": mina_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(mark(ev.clone(), mina_id.clone()).await, StatusCode::OK);
    assert_eq!(
        mark(ev.clone(), veli_id.clone()).await,
        StatusCode::BAD_REQUEST
    );

    // School audience: everyone is expected — the teacher may mark themselves.
    let ev = create(json!({ "kind": "school" })).await;
    assert_eq!(mark(ev.clone(), ali_id.clone()).await, StatusCode::OK);
    assert_eq!(mark(ev, veli_id.clone()).await, StatusCode::OK);
}

#[tokio::test]
async fn event_roster_joins_expected_with_marks() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let s1 = login(&app, "selin").await;
    let s2 = login(&app, "zeynep").await;
    let s1_id = me_id(&app, &s1).await;
    let course = create_course(&app, &ali, "algebra").await;
    enroll(&app, &ali, &course, &s1_id).await;
    enroll(&app, &ali, &course, &me_id(&app, &s2).await).await;

    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "field trip",
                     "audience": { "kind": "course", "course": course } })),
    )
    .await;
    let event_id = id_of(&ev.body);
    let roster_uri = format!("/events/{event_id}/roster");

    // Before any marks: every expected attendee, all unmarked.
    let res = send(&app, "GET", &roster_uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 2);
    assert!(
        common::items(&res.body)
            .iter()
            .all(|entry| entry["status"].is_null() && entry["marked_by"].is_null())
    );

    // Mark one: the roster shows who came and who's still missing.
    send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "present", "user_id": s1_id })),
    )
    .await;
    let res = send(&app, "GET", &roster_uri, Some(&ali), None).await;
    let items = common::items(&res.body);
    assert_eq!(items.len(), 2);
    let marked: Vec<_> = items
        .iter()
        .filter(|entry| entry["status"] == "present")
        .collect();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0]["user"]["id"], s1_id);
    assert_eq!(marked[0]["marked_by"]["username"], "ali");
    assert_eq!(
        items
            .iter()
            .filter(|entry| entry["status"].is_null())
            .count(),
        1
    );

    // The roster pages like any list.
    let res = send(
        &app,
        "GET",
        &format!("{roster_uri}?limit=1&offset=1"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(common::total(&res.body), 2);

    // Dropping someone from the audience (unenroll) drops them from the
    // roster report — their recorded mark stays in the attendance listing.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{s1_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &roster_uri, Some(&ali), None).await;
    assert_eq!(common::total(&res.body), 1);
    assert!(common::items(&res.body)[0]["status"].is_null());
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "stale mark stays recorded");

    // A missing event has no roster — 404, not an empty page.
    assert_eq!(
        send(&app, "GET", "/events/nope/roster", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// --- exams + results -----------------------------------------------------

#[tokio::test]
async fn exams_are_course_scoped_and_course_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;

    let course_id = create_course(&app, &ali, "algebra").await;
    let ex = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/exams"),
        Some(&ali),
        Some(json!({ "title": "ch3", "description": "algebra", "kind": "quiz" })),
    )
    .await;
    assert_eq!(ex.status, StatusCode::CREATED);
    assert_eq!(ex.body["kind"], "quiz");
    assert_eq!(ex.body["course"], course_id);
    // Weight lives on the kind (settings), not the exam.
    assert_eq!(ex.body.get("weight"), None);
    let exam_id = id_of(&ex.body);

    // A teacher outside the course sees none of it: not the exam, not the
    // course's exam list, and an empty catalog.
    assert_eq!(
        send(&app, "GET", &format!("/exams/{exam_id}"), Some(&veli), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/exams", Some(&veli), None).await.body).len(),
        0
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
        .status,
        StatusCode::FORBIDDEN
    );

    // The course creator reads all three views.
    assert_eq!(
        send(&app, "GET", &format!("/exams/{exam_id}"), Some(&ali), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/exams", Some(&ali), None).await.body).len(),
        1
    );
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/courses/{course_id}/exams"),
                Some(&ali),
                None
            )
            .await
            .body
        )
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

    // Course creator edits (partial) — kind flips homework, title kept.
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
    let course_id = create_course(&app, &ali, "algebra").await;
    let exams_uri = format!("/courses/{course_id}/exams");

    // Missing/blank title -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"  ","kind":"quiz"}))
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
            Some(json!({"title":"t","kind":"essay"}))
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
            Some(json!({"title":"t","kind":"final"}))
        )
        .await
        .status,
        StatusCode::CREATED
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
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;

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
        common::items(
            &send(
                &app,
                "GET",
                &format!("/exams/{exam_id}/results"),
                Some(&teacher),
                None
            )
            .await
            .body
        )
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
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "homework").await;

    // Students cannot create exams, grade anyone, list all results, or delete a result.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/exams"),
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
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "quiz").await;
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
async fn courses_are_owner_scoped_and_creator_guarded() {
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
    // The creator comes back as an embedded person ref, not a bare id — the
    // catalog renders names without a second users request.
    assert_eq!(res.body["creator"]["username"], "ali");
    let course_id = id_of(&res.body);

    // Another teacher is not enrolled and doesn't manage it: no read, and the
    // catalog shows them nothing.
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
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/courses", Some(&veli), None).await.body).len(),
        0
    );

    // The creator and a manager see it — in the catalog too.
    for caller in [&ali, &boss] {
        assert_eq!(
            send(
                &app,
                "GET",
                &format!("/courses/{course_id}"),
                Some(caller),
                None
            )
            .await
            .status,
            StatusCode::OK
        );
        let catalog = send(&app, "GET", "/courses", Some(caller), None).await.body;
        let rows = common::items(&catalog);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["creator"]["username"], "ali",
            "the catalog embeds the creator ref on every row"
        );
    }

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

/// A manager can put a second teacher in charge of someone else's course. The
/// assignee then manages everything inside it, but the course stays its
/// creator's to delete — and a demotion takes the assignment away.
#[tokio::test]
async fn assigned_teacher_manages_course_without_owning_it() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let rektor = login_as(&app, &db, "rektor", "admin").await;
    let veli_id = id_of(&send(&app, "GET", "/auth/me", Some(&veli), None).await.body);

    let course_id = id_of(
        &send(
            &app,
            "POST",
            "/courses",
            Some(&ali),
            Some(json!({ "title": "algebra" })),
        )
        .await
        .body,
    );
    let teachers = format!("/courses/{course_id}/teachers");

    // Before assignment veli is just another teacher: no read, no write.
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
        StatusCode::FORBIDDEN
    );

    // Staffing is the office's call — the course's own creator cannot do it.
    assert_eq!(
        send(
            &app,
            "POST",
            &teachers,
            Some(&ali),
            Some(json!({ "user_id": veli_id }))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Only teacher+ can be assigned.
    let student = login_as(&app, &db, "zeynep", "student").await;
    let student_id = id_of(
        &send(&app, "GET", "/auth/me", Some(&student), None)
            .await
            .body,
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &teachers,
            Some(&boss),
            Some(json!({ "user_id": student_id }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // The manager assigns veli; the course echoes its staff back.
    let res = send(
        &app,
        "POST",
        &teachers,
        Some(&boss),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["creator"]["username"], "ali");
    assert_eq!(res.body["teachers"][0]["username"], "veli");

    // Assigning twice is idempotent — no duplicate row in the list.
    let again = send(
        &app,
        "POST",
        &teachers,
        Some(&boss),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["teachers"].as_array().unwrap().len(), 1);

    // Veli now runs the course: it shows in their catalog, and they can edit
    // it and enroll students.
    let catalog = send(&app, "GET", "/courses", Some(&veli), None).await.body;
    assert_eq!(common::items(&catalog).len(), 1);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/courses/{course_id}"),
            Some(&veli),
            Some(json!({"description":"letters"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&veli),
            Some(json!({ "user_id": student_id }))
        )
        .await
        .status,
        StatusCode::OK
    );

    // But the course is not theirs to delete, nor to re-staff.
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
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("{teachers}/{veli_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Demoting veli below teacher sweeps the assignment: the course drops off
    // their catalog and the rights go with it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{veli_id}/role"),
            Some(&rektor),
            Some(json!({"role":"student"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let after = send(
        &app,
        "GET",
        &format!("/courses/{course_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        after.body["teachers"].as_array().unwrap().len(),
        0,
        "a demoted teacher is swept off the courses they were assigned to"
    );

    // Re-assign, then unassign by hand: the second removal is a 404.
    send(
        &app,
        "PATCH",
        &format!("/users/{veli_id}/role"),
        Some(&rektor),
        Some(json!({"role":"teacher"})),
    )
    .await;
    assert_eq!(
        send(
            &app,
            "POST",
            &teachers,
            Some(&boss),
            Some(json!({ "user_id": veli_id }))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("{teachers}/{veli_id}"),
            Some(&boss),
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
            &format!("{teachers}/{veli_id}"),
            Some(&boss),
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
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN,
        "unassigning takes the course back out of their reach"
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

/// A course is `course` by default, may be born a `study` (etüt), the kind is
/// PATCH-editable, and anything else is a 400.
#[tokio::test]
async fn course_kind_defaults_validates_and_edits() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    let plain = send(
        &app,
        "POST",
        "/courses",
        Some(&ali),
        Some(json!({"title":"algebra"})),
    )
    .await;
    assert_eq!(plain.status, StatusCode::CREATED);
    assert_eq!(plain.body["kind"], "course");

    let etut = send(
        &app,
        "POST",
        "/courses",
        Some(&ali),
        Some(json!({"title":"evening etut", "kind":"study"})),
    )
    .await;
    assert_eq!(etut.status, StatusCode::CREATED);
    assert_eq!(etut.body["kind"], "study");

    assert_eq!(
        send(
            &app,
            "POST",
            "/courses",
            Some(&ali),
            Some(json!({"title":"nope", "kind":"etut"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // PATCH flips the kind; omitting it keeps the current one.
    let id = id_of(&plain.body);
    let flipped = send(
        &app,
        "PATCH",
        &format!("/courses/{id}"),
        Some(&ali),
        Some(json!({"kind":"study"})),
    )
    .await;
    assert_eq!(flipped.status, StatusCode::OK);
    assert_eq!(flipped.body["kind"], "study");
    let renamed = send(
        &app,
        "PATCH",
        &format!("/courses/{id}"),
        Some(&ali),
        Some(json!({"title":"geometry"})),
    )
    .await;
    assert_eq!(renamed.body["kind"], "study", "omitted kind is kept");
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
    assert_eq!(common::items(&roster.body).len(), 1);
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
        common::items(
            &send(&app, "GET", "/courses/me", Some(&alice), None)
                .await
                .body,
        )
        .len(),
        1
    );
    assert_eq!(
        common::items(
            &send(&app, "GET", "/courses/me", Some(&bob), None)
                .await
                .body,
        )
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
async fn capacity_caps_the_roster() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    // A zero-seat club is nonsense.
    assert_eq!(
        send(
            &app,
            "POST",
            "/courses",
            Some(&teacher),
            Some(json!({ "title": "chess", "kind": "club", "capacity": 0 })),
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // A one-seat club.
    let created = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "chess", "kind": "club", "capacity": 1 })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["kind"], "club");
    assert_eq!(created.body["capacity"], 1);
    let course_id = id_of(&created.body);

    // The seat goes to alice; bob bounces off the full roster; re-enrolling
    // the member stays an idempotent OK even at the cap.
    let enroll = |cookie: String, user_id: String| {
        let app = app.clone();
        let path = format!("/courses/{course_id}/enrollments");
        async move {
            send(
                &app,
                "POST",
                &path,
                Some(&cookie),
                Some(json!({ "user_id": user_id })),
            )
            .await
            .status
        }
    };
    assert_eq!(
        enroll(teacher.clone(), alice_id.clone()).await,
        StatusCode::OK
    );
    assert_eq!(
        enroll(teacher.clone(), bob_id.clone()).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        enroll(teacher.clone(), alice_id.clone()).await,
        StatusCode::OK
    );

    // `null` lifts the cap and the door reopens.
    let lifted = send(
        &app,
        "PATCH",
        &format!("/courses/{course_id}"),
        Some(&teacher),
        Some(json!({ "capacity": null })),
    )
    .await;
    assert_eq!(lifted.status, StatusCode::OK);
    assert!(lifted.body["capacity"].is_null());
    assert_eq!(enroll(teacher.clone(), bob_id).await, StatusCode::OK);
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
            Some(json!({"title":"t","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::METHOD_NOT_ALLOWED
    );

    let course_id = create_course(&app, &teacher, "algebra").await;
    // Creation inside the course echoes the course.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/exams"),
        Some(&teacher),
        Some(json!({"title":"mt","kind":"midterm"})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["course"], course_id);

    // A teacher who doesn't manage the course cannot add exams to it.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/exams"),
            Some(&veli),
            Some(json!({"title":"x","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // An unenrolled student cannot list the course's exams …
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/exams"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // … but an enrolled one can.
    let alice_id = me_id(&app, &alice).await;
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&teacher),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/courses/{course_id}/exams"),
                Some(&alice),
                None
            )
            .await
            .body
        )
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
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;
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
        common::items(
            &send(&app, "GET", &grade_uri, Some(&teacher), None)
                .await
                .body
        )
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
    let exam_id = create_exam(&app, &boss, &course_id, "mt", "midterm").await;

    // ...and the course creator (not the exam's creator) can edit and delete it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            Some(json!({"title":"renamed"}))
        )
        .await
        .body["title"],
        "renamed"
    );
    // An unrelated teacher still cannot.
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
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;
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
async fn weighted_averages_follow_kind_weights() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    // Weights are school policy, per kind: quizzes count once, midterms three
    // times (the defaults are all 1).
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "quiz", "weight": 1},
            {"name": "midterm", "weight": 3},
            {"name": "final", "weight": 4},
            {"name": "homework", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Course A: quiz (w1, mark 50) + midterm (w3, mark 90) -> (50 + 270) / 4 = 80.
    let algebra = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &algebra, &alice_id).await;
    let quiz = create_exam(&app, &teacher, &algebra, "quiz", "quiz").await;
    let midterm = create_exam(&app, &teacher, &algebra, "midterm", "midterm").await;
    // A third exam stays ungraded and must not drag the average.
    create_exam(&app, &teacher, &algebra, "final", "final").await;
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
    create_exam(&app, &teacher, &physics, "hw", "homework").await;

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
    // Each entry carries its kind's resolved weight.
    let midterm_entry = algebra_block["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "midterm")
        .unwrap();
    assert_eq!(midterm_entry["weight"], 3);
    assert_eq!(physics_block["average"], serde_json::Value::Null);
    assert_eq!(physics_block["results"].as_array().unwrap().len(), 0);
    // Overall skips the null course instead of zeroing it.
    assert_eq!(report.body["overall_average"], 80.0);
    assert_eq!(report.body["user"], alice_id);
}

/// Kind weights resolve at report time: editing a kind's weight re-weights
/// already-graded exams, and a kind removed from settings counts once.
#[tokio::test]
async fn kind_weight_edits_reweight_reports_live() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &alice_id).await;
    let quiz = create_exam(&app, &teacher, &course, "q", "quiz").await;
    let oral = create_exam(&app, &teacher, &course, "o", "oral").await;
    // Ungraded for now — its kind gets retired below, out from under it.
    let project = create_exam(&app, &teacher, &course, "p", "project").await;
    for (exam, mark) in [(&quiz, 40), (&oral, 80)] {
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

    // Default weights (all 1): plain mean of 40 and 80.
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 60.0);

    // Triple the oral weight — the already-graded exam follows immediately:
    // (40 + 240) / 4 = 70.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "quiz", "weight": 1},
            {"name": "oral", "weight": 3},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 70.0);

    // The PATCH above also retired `project` (allowed — no marks of that kind
    // existed yet; a graded kind can't leave at all, see
    // `exam_kind_removal_blocks_while_marks_exist`). Its exam lives on and
    // counts with weight 1.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{project}/results"),
        Some(&teacher),
        Some(json!({ "mark": 60, "user_id": alice_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // (40 + 240 + 60) / 5 = 68.
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 68.0);
    let project_entry = report.body["courses"][0]["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "project")
        .unwrap()
        .clone();
    assert_eq!(project_entry["weight"], 1);
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

/// One teacher's course is a wall: another teacher can't read its roster,
/// results, statistics, live monitor, answer key, or answer sheets, and the
/// per-user reports narrow to the courses the caller manages. Managers see
/// through the wall; an enrolled student sees the course itself.
#[tokio::test]
async fn course_data_is_walled_off_from_other_teachers() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &owner, "algebra").await;
    enroll(&app, &owner, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &owner, &course_id, "mt", "quiz").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&owner),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Every course-scoped teacher read is refused for the rival …
    for uri in [
        format!("/courses/{course_id}/enrollments"),
        format!("/exams/{exam_id}/results"),
        format!("/exams/{exam_id}/statistics"),
        format!("/exams/{exam_id}/live"),
        format!("/exams/{exam_id}/questions"),
        format!("/exams/{exam_id}/attempts/{alice_id}/answers"),
    ] {
        let res = send(&app, "GET", &uri, Some(&rival), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri}");
    }

    // … and open to the owner and a manager. (The answer sheet turns into a
    // 404 once authorization clears: alice never sat the exam.)
    for caller in [&owner, &boss] {
        for uri in [
            format!("/courses/{course_id}/enrollments"),
            format!("/exams/{exam_id}/results"),
            format!("/exams/{exam_id}/statistics"),
            format!("/exams/{exam_id}/live"),
            format!("/exams/{exam_id}/questions"),
        ] {
            let res = send(&app, "GET", &uri, Some(caller), None).await;
            assert_eq!(res.status, StatusCode::OK, "{uri}");
        }
        let res = send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/attempts/{alice_id}/answers"),
            Some(caller),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::NOT_FOUND);
    }

    // The marks report narrows to the caller's courses: the rival gets an
    // empty shell, the owner and the manager get the graded course.
    let res = send(
        &app,
        "GET",
        &format!("/marks/{alice_id}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["courses"].as_array().unwrap().is_empty());
    assert!(res.body["overall_average"].is_null());
    for caller in [&owner, &boss] {
        let res = send(
            &app,
            "GET",
            &format!("/marks/{alice_id}"),
            Some(caller),
            None,
        )
        .await;
        assert_eq!(res.body["courses"].as_array().unwrap().len(), 1);
        assert_eq!(res.body["overall_average"], 70.0);
    }

    // The enrolled student reads the course and its exam, and both catalogs
    // include them; the rival's catalogs stay empty; a manager sees all.
    for uri in [format!("/courses/{course_id}"), format!("/exams/{exam_id}")] {
        let res = send(&app, "GET", &uri, Some(&alice), None).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}");
    }
    for (caller, visible) in [(&alice, 1), (&rival, 0), (&boss, 1)] {
        for uri in ["/courses", "/exams"] {
            let res = send(&app, "GET", uri, Some(caller), None).await;
            assert_eq!(
                common::items(&res.body).len(),
                visible,
                "{uri} as seen by caller"
            );
        }
    }
}

#[tokio::test]
async fn deleting_course_cascades_enrollments_exams_and_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;

    unenroll(&app, &teacher, &course_id, &alice_id).await;
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
        common::items(
            &send(&app, "GET", "/courses/me", Some(&alice), None)
                .await
                .body,
        )
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
        common::items(&roster.body).len(),
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
async fn students_never_mark_attendance() {
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

    // Taking attendance is teacher+: a student cannot mark even themselves…
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
        StatusCode::FORBIDDEN
    );
    // …let alone someone else.
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
    // And cannot remove attendance rows or read the roster report.
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
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{ev}/roster"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

/// Registration seats follow the placement rule — teachers seat students and
/// themselves, never other staff; students never touch the endpoints — and a
/// repeat register is a no-op on the same single row. Deleting the event
/// removes its signup rows with it.
#[tokio::test]
async fn event_registration_follows_placement_rules() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let deniz = login_as(&app, &db, "deniz", "teacher").await;
    let veli = login(&app, "veli").await; // student
    let veli_id = me_id(&app, &veli).await;
    let ali_id = me_id(&app, &ali).await;
    let deniz_id = me_id(&app, &deniz).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "seminar", "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let ev = id_of(&res.body);
    let register_uri = format!("/events/{ev}/register");

    // Students can't register — not even themselves.
    let res = send(&app, "POST", &register_uri, Some(&veli), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A teacher seats a student; the row names who placed whom.
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["user"]["id"], veli_id);
    assert_eq!(res.body["registered_by"]["id"], ali_id);

    // Registering the same student again is a no-op on the same seat.
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&deniz),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let roster = send(
        &app,
        "GET",
        &format!("/events/{ev}/roster"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&roster.body), 1, "upsert keeps a single seat");

    // A teacher takes their own seat by omitting the target…
    let res = send(&app, "POST", &register_uri, Some(&ali), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::OK);
    // …but never seats another staff member, and ghosts don't register.
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&ali),
        Some(json!({ "user_id": deniz_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&ali),
        Some(json!({ "user_id": "ghost" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Non-registration events refuse the endpoint outright.
    let school = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "assembly" })),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/events/{}/register", id_of(&school.body)),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Unregistering mirrors the rule: students never, staff seats are their
    // own, students' seats are anyone's (teacher+) to free.
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{ali_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{ali_id}"),
        Some(&deniz),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "staff seat is not deniz's"
    );
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{veli_id}"),
        Some(&deniz),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{veli_id}"),
        Some(&deniz),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "seat already freed");

    // Deleting the event cascades its signup rows away.
    let res = send(&app, "DELETE", &format!("/events/{ev}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let left: Vec<hezarfen_backend::domain::registration::Registration> =
        db.select("registration").await.unwrap();
    assert!(left.is_empty(), "event delete leaves no seats behind");
}

/// A capacity caps live seats, not history: re-registering never double-counts,
/// freeing a seat reopens it, and shrinking the cap below the current count
/// only blocks new seats.
#[tokio::test]
async fn event_registration_capacity_caps_seats() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let mina = login(&app, "mina").await;
    let veli_id = me_id(&app, &veli).await;
    let mina_id = me_id(&app, &mina).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "workshop",
                     "audience": { "kind": "registration", "capacity": 1 } })),
    )
    .await;
    let ev = id_of(&res.body);
    let register_uri = format!("/events/{ev}/register");
    let register = |user: String| {
        let app = &app;
        let ali = &ali;
        let uri = &register_uri;
        async move {
            send(
                app,
                "POST",
                uri,
                Some(ali),
                Some(json!({ "user_id": user })),
            )
            .await
            .status
        }
    };

    assert_eq!(register(veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        register(mina_id.clone()).await,
        StatusCode::CONFLICT,
        "full"
    );
    assert_eq!(
        register(veli_id.clone()).await,
        StatusCode::OK,
        "re-register of a seated user never counts against the cap"
    );

    // Freeing the seat reopens the list.
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{veli_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(register(mina_id.clone()).await, StatusCode::OK);

    // Shrinking the cap under the current count keeps the seats and only
    // refuses growth.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{ev}"),
            Some(&ali),
            Some(json!({ "audience": { "kind": "registration", "capacity": 2 } })),
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(register(veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{ev}"),
            Some(&ali),
            Some(json!({ "audience": { "kind": "registration", "capacity": 1 } })),
        )
        .await
        .status,
        StatusCode::OK
    );
    let roster = send(
        &app,
        "GET",
        &format!("/events/{ev}/roster"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        common::total(&roster.body),
        2,
        "shrink keeps existing seats"
    );
}

/// Regression: re-registering an already-seated student used to be an UPSERT
/// that silently rewrote `registered_by` to the second caller. The documented
/// contract is a no-op — the seat and its audit trail come back unchanged.
#[tokio::test]
async fn re_registering_returns_the_seat_untouched() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let deniz = login_as(&app, &db, "deniz", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "seminar", "audience": { "kind": "registration" } })),
    )
    .await;
    let uri = format!("/events/{}/register", id_of(&res.body));

    let first = send(
        &app,
        "POST",
        &uri,
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body["registered_by"]["id"], ali_id);

    let second = send(
        &app,
        "POST",
        &uri,
        Some(&deniz),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        second.body["registered_by"]["id"], ali_id,
        "re-registering must not rewrite who placed the student"
    );
    // Regression: the response used to resolve people only from {target,
    // caller}, so the original registrar degraded to a bare ULID username.
    assert_eq!(
        second.body["registered_by"]["username"], "ali",
        "the original registrar must resolve to a person, not a bare id"
    );
    assert_eq!(second.body["user"]["username"], "veli");
}

/// Regression: the capacity check used to run inside a `BEGIN…COMMIT` whose
/// cross-record `count()` SurrealDB does not conflict-check against a
/// concurrent insert (write-skew) — two racing placements both saw the last
/// seat free and a capacity-1 event admitted 2. The register path now
/// serializes the check-then-write, so exactly one wins and the loser gets
/// the same 409 as a sequential over-fill.
#[tokio::test]
async fn concurrent_placements_never_exceed_capacity() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse_id = me_id(&app, &ayse).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "trip", "audience": { "kind": "registration", "capacity": 1 } })),
    )
    .await;
    let ev = id_of(&res.body);
    let uri = format!("/events/{ev}/register");

    for _round in 0..10 {
        for uid in [&veli_id, &ayse_id] {
            let _ = send(&app, "DELETE", &format!("{uri}/{uid}"), Some(&ali), None).await;
        }
        let (a, b) = tokio::join!(
            send(
                &app,
                "POST",
                &uri,
                Some(&ali),
                Some(json!({ "user_id": veli_id })),
            ),
            send(
                &app,
                "POST",
                &uri,
                Some(&ali),
                Some(json!({ "user_id": ayse_id })),
            ),
        );
        for status in [a.status, b.status] {
            assert!(
                status == StatusCode::OK || status == StatusCode::CONFLICT,
                "a lost capacity race must be a 409, got {status}"
            );
        }
        let roster = send(
            &app,
            "GET",
            &format!("/events/{ev}/roster"),
            Some(&ali),
            None,
        )
        .await;
        assert!(
            common::total(&roster.body) <= 1,
            "capacity 1 must never over-admit, roster={}",
            roster.body
        );
    }
}

/// The signup list freezes the moment the event starts — both directions.
/// (The create-time no-past grace lets a just-started event exist, which is
/// exactly a closed list.)
#[tokio::test]
async fn event_registration_closes_at_start() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let now = Timestamp::now().as_millis();

    // Started 30s ago: inside the scheduling grace, so creation passes — but
    // the list is already closed.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "underway", "starts_at": now - 30_000,
                     "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let ev = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/events/{ev}/register"),
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = send(
        &app,
        "DELETE",
        &format!("/events/{ev}/register/{veli_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "unregister freezes too");

    // Regression: an ends_at-only event (a pure signup deadline) used to stay
    // open forever — the gate read starts_at alone. Ended 30s ago: inside the
    // creation grace, but the list is closed.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "deadline passed", "ends_at": now - 30_000,
                     "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let ev = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/events/{ev}/register"),
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "an event whose end has passed takes no signups"
    );

    // A future or timeless event keeps its list open — a future signup
    // deadline (ends_at only) included.
    for body in [
        json!({ "title": "later", "starts_at": now + 3_600_000,
                "audience": { "kind": "registration" } }),
        json!({ "title": "deadline ahead", "ends_at": now + 3_600_000,
                "audience": { "kind": "registration" } }),
        json!({ "title": "whenever", "audience": { "kind": "registration" } }),
    ] {
        let res = send(&app, "POST", "/events", Some(&ali), Some(body)).await;
        let ev = id_of(&res.body);
        let res = send(
            &app,
            "POST",
            &format!("/events/{ev}/register"),
            Some(&ali),
            Some(json!({ "user_id": veli_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
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
    let admin = login_as(&app, &db, "boss", "admin").await;
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
    assert!(common::items(&list.body).len() >= 2);

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
    let admin = login_as(&app, &db, "boss", "admin").await;
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
    let listed = common::items(&list.body)
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

// --- users: UI preferences (theme, language) ------------------------------

#[tokio::test]
async fn preferences_start_null_and_update_via_me_preferences() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    // A fresh account has never chosen — both come back null.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert!(me.body["theme"].is_null(), "theme should start null");
    assert!(me.body["language"].is_null(), "language should start null");

    // Set both through the self-service endpoint.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "theme": "dark", "language": "tr" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["theme"], "dark");
    assert_eq!(res.body["language"], "tr");

    // Partial patch: only the theme changes, the language survives.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "theme": "light" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["theme"], "light");
    assert_eq!(res.body["language"], "tr");

    // Empty string clears back to "never chose"; the other stays.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "theme": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["theme"].is_null());
    assert_eq!(res.body["language"], "tr");

    // The merged state is what /auth/me reports afterwards.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert!(me.body["theme"].is_null());
    assert_eq!(me.body["language"], "tr");

    // No session -> 401.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/me/preferences",
            None,
            Some(json!({"theme":"dark"}))
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn preferences_reject_invalid_values() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    let bad = [
        json!({ "theme": "solarized" }),
        json!({ "theme": "Dark" }),
        json!({ "language": "turkish" }),
        json!({ "language": "de" }),
        json!({ "theme": "dark", "language": "nope" }),
    ];
    for body in bad {
        let res = send(
            &app,
            "PATCH",
            "/users/me/preferences",
            Some(&bob),
            Some(body.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // A rejected patch must not have half-applied: both are still null.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert!(me.body["theme"].is_null(), "theme should still be null");
    assert!(
        me.body["language"].is_null(),
        "language should still be null"
    );
}

#[tokio::test]
async fn admin_edits_any_preferences_with_guards() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let alice = login(&app, "alice").await;
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);
    let admin_id = id_of(&send(&app, "GET", "/auth/me", Some(&admin), None).await.body);

    // A student cannot touch someone else's preferences.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{admin_id}/preferences"),
            Some(&alice),
            Some(json!({"theme":"dark"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Admin sets alice's preferences on her behalf.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/preferences"),
        Some(&admin),
        Some(json!({ "theme": "dark", "language": "en" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["theme"], "dark");
    assert_eq!(res.body["language"], "en");

    // Admin reads them on the single-user lookup; alice sees them on /auth/me.
    let one = send(
        &app,
        "GET",
        &format!("/users/{alice_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(one.body["theme"], "dark");
    assert_eq!(one.body["language"], "en");
    let me = send(&app, "GET", "/auth/me", Some(&alice), None).await;
    assert_eq!(me.body["theme"], "dark");
    assert_eq!(me.body["language"], "en");

    // Unknown id -> 404.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/does-not-exist/preferences",
            Some(&admin),
            Some(json!({"theme":"dark"}))
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
        common::items(&roster.body).len(),
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
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "quiz").await;

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
        common::items(&results.body).len(),
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
    let (app, db) = app_and_db().await;

    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": "ali", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Padding must not mint a lookalike account. Register answers 201 either way
    // (username enumeration is denied, see tests/auth_enumeration.rs), so the
    // collision is observed through the account itself: each spoof asks for a
    // different password, and none of them may take effect.
    for spoof in ["ali ", " ali", "  ali  "] {
        let res = send(
            &app,
            "POST",
            "/auth/register",
            None,
            Some(json!({ "username": spoof, "password": "spoof1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{spoof:?}");
    }

    // One row, not four — the padded forms collided with the canonical name.
    let rows: Vec<User> = db
        .query("SELECT * FROM user WHERE username = 'ali'")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(rows.len(), 1, "padding minted a lookalike account");
    let all: Vec<User> = db
        .query("SELECT * FROM user")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(all.len(), 1, "unexpected extra user rows");

    // That one account still belongs to the first registration, and every
    // spelling of the name resolves to it.
    for attempt in ["ali", "ali ", " ali", "  ali  "] {
        let res = send(
            &app,
            "POST",
            "/auth/login",
            None,
            Some(json!({ "username": attempt, "password": "secret1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "login as {attempt:?}");
        let res = send(
            &app,
            "POST",
            "/auth/login",
            None,
            Some(json!({ "username": attempt, "password": "spoof1" })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "a padded register overwrote \"ali\" (as {attempt:?})"
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
/// could write their own mark. "Grading never targets oneself" (README). The
/// self-check precedes the target-role and enrollment checks, so it holds even
/// though staff — the only ones who can grade — are never enrolled or
/// gradeable themselves.
#[tokio::test]
async fn graders_cannot_grade_themselves() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let teacher_id = me_id(&app, &teacher).await;
    let boss_id = me_id(&app, &boss).await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &student_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "t", "quiz").await;

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
        common::items(
            &send(
                &app,
                "GET",
                &format!("/exams/{exam_id}/results"),
                Some(&teacher),
                None
            )
            .await
            .body
        )
        .len(),
        0
    );

    // Grading someone *else* still works (boss grades the enrolled student).
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&boss),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// Enrollment is student membership: staff (teacher/manager/admin) can't be
/// enrolled, so they never gain the seat that gates sitting exams, being
/// graded, and the class roster. A plain student enrolls fine.
#[tokio::test]
async fn only_students_can_be_enrolled() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let colleague = login_as(&app, &db, "colleague", "teacher").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let colleague_id = me_id(&app, &colleague).await;
    let manager_id = me_id(&app, &manager).await;

    let course = create_course(&app, &teacher, "algebra").await;

    // A fellow teacher and a manager are both refused — staff don't enroll.
    for staff_id in [&colleague_id, &manager_id] {
        let res = send(
            &app,
            "POST",
            &format!("/courses/{course}/enrollments"),
            Some(&teacher),
            Some(json!({ "user_id": staff_id })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "enrolling staff must be rejected"
        );
    }

    // A student enrolls without complaint (the helper asserts 200).
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &teacher, &course, &student_id).await;
}

/// The student-only rule is enforced on the *live* role, not merely at enroll
/// time. This flips the role directly in the DB (bypassing the admin endpoint,
/// which also sweeps enrollments — and boot sweeps rows like this one), so the
/// enrollment row is still present — but inert: they can no longer sit the
/// exam, save answers, be graded, or be marked present. The action-time checks
/// alone hold, with no help from roster hygiene.
#[tokio::test]
async fn promotion_out_of_student_freezes_the_seat() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let future = Timestamp::now().as_millis() + 3_600_000;
    let session = create_session(&app, &teacher, &course, future).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "essay", "kind": "quiz", "mode": "open" }),
    )
    .await;

    // While still a student, the seat works: they can start a sitting.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "student sits: {}",
        res.body
    );

    // Promote the enrolled student to teacher; the enrollment row survives.
    set_role(&db, "veli", "teacher").await;

    // Sitting is now refused — the live role, not the stale seat, decides.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "promoted user can't start a sitting"
    );

    // Nor can they save into the attempt they opened while still a student.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": "nope", "selected": "c0" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "promoted user can't save answers"
    );

    // Grading the promoted user is refused — only students carry marks.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 80, "user_id": student_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "promoted user can't be graded"
    );

    // Roll call is refused too — only students attend classes.
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&teacher),
        Some(json!({ "status": "present", "user_id": student_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "promoted user can't be rolled"
    );
}

/// Promotion through the admin role endpoint doesn't just freeze the seat —
/// it deletes the user's enrollment rows outright, so a promoted user drops
/// off course rosters and live-monitor counts instead of lingering.
#[tokio::test]
async fn promotion_via_role_endpoint_sweeps_enrollments() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&roster.body), 1);

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{student_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::total(&roster.body),
        0,
        "promoted user left the roster: {}",
        roster.body
    );

    // Really deleted, not merely filtered out of the listing.
    let mut result = db
        .query("SELECT VALUE id FROM enrollment")
        .await
        .expect("count enrollments")
        .check()
        .expect("count enrollments check");
    let rows: Vec<surrealdb::types::RecordId> = result.take(0).expect("enrollment rows");
    assert!(rows.is_empty(), "enrollment rows deleted from the DB");
}

/// Regression: `User::create` pre-checks the username and then inserts, so two
/// concurrent registrations of the same name could both pass the check; the
/// loser then hit the unique index and surfaced as a raw 500. A lost race must
/// answer the same uniform 201 as the winner (the "taken" reply is deliberately
/// indistinguishable), exactly one account may exist afterwards, and the
/// winner's password must be the one that works.
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

    for h in handles {
        let status = h.await.unwrap();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "a lost race must answer 201 like the winner, not a 500"
        );
    }

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

    // And the winner's credentials survived the pile-up: a further duplicate
    // asking for a different password does not replace them.
    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": "dup", "password": "hijack1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": "dup", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "the winning password broke");
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": "dup", "password": "hijack1" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::UNAUTHORIZED,
        "a duplicate register overwrote the account"
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
    // SameSite=Lax is the cookie-side half of the CORS defense: dev mirror
    // mode (see `cors_layer`) is only safe as long as this cookie never rides
    // cross-site requests.
    assert!(
        plain.contains("SameSite=Lax"),
        "cookie must stay SameSite=Lax: {plain}"
    );

    // With the flag on: Secure present.
    let db = database::init_mem().await.unwrap();
    let app = build_router(AppState {
        db,
        files_path: common::files_dir(),
        cookie_secure: true,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        db_up: Default::default(),
        ai: None,
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

/// A query issued while the database socket is down does not fail — the SDK
/// parks it until the connection returns and *then* runs it, so a handler that
/// reaches the database mid-outage waits out the whole outage and any write it
/// carries lands late. The guard layer refuses at the edge instead, before the
/// request can touch the database, which is what makes the 503 safe to retry.
#[tokio::test]
async fn db_down_refuses_before_touching_the_database() {
    async fn count_rows(db: &hezarfen_backend::database::Database, table: &str) -> usize {
        let mut rows = db
            .query(format!("SELECT id FROM {table}"))
            .await
            .expect("count query");
        let ids: Vec<surrealdb::types::RecordId> = rows.take(0).expect("ids");
        ids.len()
    }

    let db = database::init_mem().await.unwrap();
    let db_up = hezarfen_backend::state::DbHealth::default();
    let app = build_router(AppState {
        db: db.clone(),
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        db_up: db_up.clone(),
        ai: None,
    });

    // Healthy: a login against the seeded user reaches the handler as usual.
    let before: usize = count_rows(&db, "user").await;
    let ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"username": "gulsah", "password": "sifre12345"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::CREATED);
    assert_eq!(
        count_rows(&db, "user").await,
        before + 1,
        "the write landed"
    );

    // Socket reported down: refused with a retryable 503, and — the point of
    // refusing at the edge rather than timing out — nothing was written.
    db_up.set(false);
    let refused = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"username": "kerem", "password": "sifre12345"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refused.headers().get("retry-after").unwrap(),
        "1",
        "a pre-execution refusal must advertise that retrying is safe"
    );
    assert_eq!(
        count_rows(&db, "user").await,
        before + 1,
        "a refused request must not have reached the database"
    );

    // Recovery flips back without a restart.
    db_up.set(true);
    let healed = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"username": "kerem", "password": "sifre12345"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(healed.status(), StatusCode::CREATED);
    assert_eq!(count_rows(&db, "user").await, before + 2);
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

/// #4 — mirror mode (no `CORS_ALLOWED_ORIGINS`) reflects the caller's origin,
/// never `*`, but must NOT advertise credentials: reflecting arbitrary origins
/// with credentials would let any website ride a visitor's session cookie.
#[tokio::test]
async fn cors_mirror_mode_reflects_origin_without_credentials() {
    let app = mem_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let headers = res.headers();
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://evil.example",
        "origin must be reflected, not wildcarded"
    );
    // tower-http omits the header entirely when credentials are off.
    assert!(
        headers.get("access-control-allow-credentials").is_none(),
        "mirror mode must never send allow-credentials"
    );
}

/// Allowlist mode: a listed origin gets both the origin echo and
/// `allow-credentials: true`; an unlisted origin gets no origin header at all.
/// `cors_layer` takes the allowlist as a parameter, so this needs no
/// process-global env mutation (which would race other tests).
#[tokio::test]
async fn cors_allowlist_mode_credits_only_listed_origins() {
    async fn probe(origin: &str) -> axum::http::HeaderMap {
        let app = axum::Router::new()
            .route("/ping", axum::routing::get(|| async { "pong" }))
            .layer(hezarfen_backend::cors_layer(vec![
                "https://app.example".parse().unwrap(),
            ]));
        let req = Request::builder()
            .method("GET")
            .uri("/ping")
            .header("origin", origin)
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().headers().clone()
    }

    let listed = probe("https://app.example").await;
    assert_eq!(
        listed.get("access-control-allow-origin").unwrap(),
        "https://app.example",
    );
    assert_eq!(
        listed.get("access-control-allow-credentials").unwrap(),
        "true",
        "the allowlisted origin must keep credentialed CORS"
    );

    let unlisted = probe("https://evil.example").await;
    assert!(
        unlisted.get("access-control-allow-origin").is_none(),
        "an unlisted origin must not be echoed"
    );
}

/// Both CORS modes share one layer base, so a single probe proves the exposed
/// headers: without them, cross-origin JS reads `null` for the documented
/// `Retry-After` (429s) and `Content-Disposition` (download filename) headers.
#[tokio::test]
async fn cors_exposes_retry_after_and_content_disposition() {
    let app = mem_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("origin", "https://app.example")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.headers().get("access-control-expose-headers").unwrap(),
        "retry-after,content-disposition",
        "contract headers must be readable by cross-origin JS"
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
    let listed = common::items(&res.body);
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
            "title": "midterm", "kind": "midterm",
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
            "title": "takehome", "kind": "quiz",
            "mode": "async", "starts_at": now, "ends_at": now + 7_200_000,
            "duration_ms": 5_400_000,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "async");
    assert_eq!(res.body["duration_ms"].as_i64(), Some(5_400_000));

    // Async: a duration exactly as long as the window is fine — only
    // strictly exceeding it is rejected (see "duration exceeds window" below).
    scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({
            "title": "exact-fit", "kind": "quiz",
            "mode": "async", "starts_at": now, "ends_at": now + 3_600_000,
            "duration_ms": 3_600_000,
        }),
    )
    .await;

    // Unscheduled: every schedule field stays null (pre-schedule behavior),
    // and the attempt policy shows its defaults.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "homework", "kind": "homework" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["mode"].is_null());
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
    assert!(res.body["duration_ms"].is_null());
    assert_eq!(res.body["max_attempts"], 1);
    assert_eq!(res.body["allow_rejoin"], true);

    // Open: no window at all; an optional per-attempt duration is fine, and
    // the attempt policy echoes what was asked for.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "practice", "kind": "quiz", "mode": "open",
            "duration_ms": 5_400_000, "max_attempts": 0, "allow_rejoin": false,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "open");
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
    assert_eq!(res.body["duration_ms"].as_i64(), Some(5_400_000));
    assert_eq!(res.body["max_attempts"], 0);
    assert_eq!(res.body["allow_rejoin"], false);

    // Inconsistent schedules are rejected as a unit.
    for (label, body) in [
        (
            "unknown mode",
            json!({ "title": "x", "kind": "quiz", "mode": "weekly",
                    "starts_at": now, "ends_at": now + 1_000 }),
        ),
        (
            "times without mode",
            json!({ "title": "x", "kind": "quiz", "starts_at": now }),
        ),
        (
            "duration without mode",
            json!({ "title": "x", "kind": "quiz", "duration_ms": 60_000 }),
        ),
        (
            "sync missing ends_at",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now }),
        ),
        (
            "sync with duration",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now, "ends_at": now + 1_000, "duration_ms": 60_000 }),
        ),
        (
            "async without duration",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 1_000 }),
        ),
        (
            "backwards window",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now + 2_000, "ends_at": now + 1_000 }),
        ),
        (
            "empty window",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now, "ends_at": now }),
        ),
        (
            "duration too short",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 90_000_000, "duration_ms": 59_999 }),
        ),
        (
            "duration too long",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 90_000_000, "duration_ms": 86_400_001 }),
        ),
        (
            // Within the global 1min-24h bounds, but longer than its own
            // window: a 90-minute duration inside a 60-minute window.
            "duration exceeds window",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 3_600_000, "duration_ms": 5_400_000 }),
        ),
        (
            "past starts_at",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now - 3_600_000, "ends_at": now + 3_600_000 }),
        ),
        (
            // starts_at is future so the rejection can only come from ends_at.
            "past ends_at",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now + 3_600_000, "ends_at": now - 3_600_000 }),
        ),
        (
            "open with a window",
            json!({ "title": "x", "kind": "quiz", "mode": "open",
                    "starts_at": now + 60_000, "ends_at": now + 120_000 }),
        ),
        (
            "open with only an ends_at",
            json!({ "title": "x", "kind": "quiz", "mode": "open",
                    "ends_at": now + 120_000 }),
        ),
        (
            "negative attempt limit",
            json!({ "title": "x", "kind": "quiz", "max_attempts": -1 }),
        ),
        (
            "attempt limit over the cap",
            json!({ "title": "x", "kind": "quiz", "max_attempts": 101 }),
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
        json!({ "title": "running", "kind": "quiz",
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
        json!({ "title": "graced", "kind": "quiz",
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
        json!({ "title": "final", "kind": "final",
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

/// Once the window closes, an enrolled student who never sat turns from
/// `not_started` into `absent` on the monitor — started-but-unsubmitted stays
/// `expired`, and an open exam (no window) never flags anyone.
#[tokio::test]
async fn closed_window_flags_no_shows_as_absent() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "abs_t", "teacher").await;
    let ayse = login(&app, "ayse").await; // never shows up
    let veli = login(&app, "veli").await; // submits
    let cem = login(&app, "cem").await; // starts, never submits
    let ayse_id = me_id(&app, &ayse).await;
    let veli_id = me_id(&app, &veli).await;
    let cem_id = me_id(&app, &cem).await;
    let course = create_course(&app, &teacher, "history").await;
    enroll(&app, &teacher, &course, &ayse_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;
    enroll(&app, &teacher, &course, &cem_id).await;

    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "midterm", "kind": "midterm",
                "mode": "sync", "starts_at": now - 50_000, "ends_at": now + 600_000 }),
    )
    .await;

    for cookie in [&veli, &cem] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Window still open: the no-show is merely `not_started`.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["not_started"], 1);
    assert_eq!(res.body["counts"]["absent"], 0);
    assert_eq!(res.body["students"][0]["user"]["username"], "ayse");
    assert_eq!(res.body["students"][0]["status"], "not_started");

    // The teacher cuts the window short (inside the backdating grace): the
    // exam is over, and the gap hardens into an absence.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "ends_at": now - 10_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["enrolled"], 3);
    assert_eq!(res.body["counts"]["not_started"], 0);
    assert_eq!(res.body["counts"]["absent"], 1);
    assert_eq!(res.body["counts"]["submitted"], 1);
    assert_eq!(res.body["counts"]["expired"], 1);
    let students = res.body["students"].as_array().expect("students");
    assert_eq!(students[0]["user"]["username"], "ayse");
    assert_eq!(students[0]["status"], "absent");
    assert_eq!(students[1]["user"]["username"], "cem");
    assert_eq!(students[1]["status"], "expired");
    assert_eq!(students[2]["user"]["username"], "veli");
    assert_eq!(students[2]["status"], "submitted");

    // Absence is informational — the mark stays a human call, and the
    // teacher may still record one for the no-show.
    assert!(students[0]["mark"].is_null());
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 0, "user_id": ayse_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // An open exam has no window: never started is never absent.
    let open = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "essay", "kind": "homework", "mode": "open" }),
    )
    .await;
    let res = send(
        &app,
        "GET",
        &format!("/exams/{open}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["not_started"], 3);
    assert_eq!(res.body["counts"]["absent"], 0);
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
        json!({ "title": "quiz", "kind": "quiz",
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

    // Tight window: starting partway through it still lets `ends_at` clamp
    // the personal deadline (duration_ms can be at most the full window now,
    // so the clamp only bites once a student starts late inside it).
    let starts = now - 40_000;
    let ends = now + 20_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz2", "kind": "quiz",
                "mode": "async", "starts_at": starts, "ends_at": ends,
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
    let unscheduled = create_exam(&app, &teacher, &course, "hw", "homework").await;
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
        json!({ "title": "later", "kind": "quiz",
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
        json!({ "title": "gone", "kind": "quiz",
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
        json!({ "title": "open", "kind": "quiz",
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
        json!({ "title": "final", "kind": "final",
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
        json!({ "title": "blitz", "kind": "quiz",
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
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends }),
    )
    .await;

    // Before any attempt the whole schedule is editable — even the mode...
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "async", "duration_ms": 60_000 })),
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
        Some(json!({ "mode": "async", "duration_ms": 60_000 })),
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
    let schedule = json!({ "title": "final", "kind": "final",
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
    unenroll(&app, &teacher, &course, &student_id).await;
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

/// An `open` exam is sittable anytime — no window, no deadline — and
/// `max_attempts` meters the retakes: every terminal attempt can be followed
/// by a fresh sitting (blank answer sheet) until the limit is spent, and the
/// limit is live-editable, `0` meaning unlimited.
#[tokio::test]
async fn open_exams_sit_anytime_and_retakes_respect_the_limit() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "open_t", "teacher").await;
    let student = login(&app, "omer").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "practice").await;
    let subject = create_subject(&app, &teacher, &course, "drills").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "drill", "kind": "quiz",
                "mode": "open", "max_attempts": 2 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);

    // First sitting starts right away — no window to wait for — and runs
    // without a deadline: it can only end by submission.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 1);
    assert_eq!(res.body["attempts_used"], 1);
    assert_eq!(res.body["max_attempts"], 2);
    assert_eq!(res.body["status"], "in_progress");
    assert!(res.body["deadline"].is_null());
    assert!(res.body["remaining_ms"].is_null());

    // Answer, then re-"start": the running attempt resumes (200), same clock.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["attempt"], 1);
    assert_eq!(res.body["answered"], 1);

    // Submit, then start again: sitting #2, and the retake wiped the sheet.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 2);
    assert_eq!(res.body["attempts_used"], 2);
    assert_eq!(res.body["answered"], 0, "retake starts from a blank sheet");
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body[0]["answer"].is_null(),
        "wiped answer resurfaced: {}",
        res.body
    );

    // The limit is spent after sitting #2 ends: no third start.
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
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // …until the teacher raises it live. `0` = unlimited: sitting #3 opens.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "max_attempts": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["max_attempts"], 0);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 3);
    assert_eq!(res.body["max_attempts"], 0);

    // Lowering the limit below what's used never kills the running attempt —
    // it only blocks the next start once this one is over.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "max_attempts": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "in_progress");
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
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// An `open` exam with a `duration_ms` gives each sitting its own countdown
/// (`started_at + duration`), and retakes inside a sync window restart the
/// sheet while the monitor tracks the latest sitting.
#[tokio::test]
async fn open_duration_and_sync_retakes_shape_the_deadline_and_monitor() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "dur_t", "teacher").await;
    let student = login(&app, "duru").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "timing").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // Open + duration: the deadline is exactly start + budget.
    let timed = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "sprint", "kind": "quiz",
                "mode": "open", "duration_ms": 60_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{timed}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let started_at = res.body["started_at"].as_i64().expect("started_at");
    assert_eq!(res.body["deadline"].as_i64(), Some(started_at + 60_000));
    let remaining = res.body["remaining_ms"].as_i64().expect("remaining_ms");
    assert!(
        remaining > 0 && remaining <= 60_000,
        "remaining {remaining}"
    );

    // Sync exam with retakes: sitting #2 opens inside the window and the
    // monitor shows the latest sitting plus the usage count.
    let now = Timestamp::now().as_millis();
    let ends = now + 600_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends,
                "max_attempts": 3 }),
    )
    .await;
    for _ in 0..2 {
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
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/finish"),
            Some(&student),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["submitted"], 1);
    let row = &res.body["students"][0];
    assert_eq!(row["user"]["username"], "duru");
    assert_eq!(row["attempt"], 2);
    assert_eq!(row["attempts_used"], 2);
    assert!(row["left_at"].is_null());

    // The third sitting is the last: the window still gates retakes, so once
    // it shuts (or the limit is spent) starting conflicts.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["attempt"], 3);
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
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

// --- subjects ---------------------------------------------------------------

#[tokio::test]
async fn subject_crud_validation_and_rbac() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sub_t", "teacher").await;
    let other_teacher = login_as(&app, &db, "sub_t2", "teacher").await;
    let boss = login_as(&app, &db, "sub_m", "manager").await;
    let student = login(&app, "selin").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "calculus").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // Create echoes the full shape; description defaults to empty.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&teacher),
        Some(json!({ "name": "limits", "description": "epsilon-delta" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["course"], course);
    assert_eq!(res.body["name"], "limits");
    assert_eq!(res.body["description"], "epsilon-delta");
    let subject = id_of(&res.body);
    let second = create_subject(&app, &teacher, &course, "derivatives").await;

    // Validation: a blank name is a 400, a missing course a 404.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&teacher),
        Some(json!({ "name": "  " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        "/courses/missing/subjects",
        Some(&teacher),
        Some(json!({ "name": "orphan" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Writes are management-gated: students lack the role, a rival teacher
    // the rights; a manager passes everywhere.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&student),
        Some(json!({ "name": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&other_teacher),
        Some(json!({ "name": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/subjects/{subject}"),
        Some(&other_teacher),
        Some(json!({ "name": "hijack" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{second}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Reads follow course visibility: the enrolled student and a manager see
    // the list and the subject, the rival teacher sees neither.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/subjects"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let listed = common::items(&res.body);
    assert_eq!(listed.len(), 2, "creation order");
    assert_eq!(id_of(&listed[0]), subject);
    assert_eq!(id_of(&listed[1]), second);
    assert_eq!(common::total(&res.body), 2);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "limits");
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/subjects"),
        Some(&other_teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject}"),
        Some(&other_teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // PATCH keeps omitted fields; a manager may edit anyone's.
    let res = send(
        &app,
        "PATCH",
        &format!("/subjects/{subject}"),
        Some(&boss),
        Some(json!({ "name": "limits & continuity" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["name"], "limits & continuity");
    assert_eq!(res.body["description"], "epsilon-delta", "kept by omission");

    // Unknown ids are 404s.
    for (method, body) in [
        ("GET", None),
        ("PATCH", Some(json!({ "name": "x" }))),
        ("DELETE", None),
    ] {
        let res = send(&app, method, "/subjects/missing", Some(&teacher), body).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method}");
    }

    // Delete works while nothing references the subject.
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{second}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{second}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subject_delete_blocks_while_questions_reference_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sdb_t", "teacher").await;
    let course = create_course(&app, &teacher, "history").await;
    let tagged = create_subject(&app, &teacher, &course, "antiquity").await;
    let spare = create_subject(&app, &teacher, &course, "middle ages").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &tagged,
        json!({ "text": "When?", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);

    // Referenced: the delete is a conflict, and the subject survives.
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{tagged}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{tagged}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Re-tag the question elsewhere: the old subject frees up, the new one
    // locks.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        Some(json!({ "subject_id": spare })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{tagged}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{spare}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // Deleting the question releases the last reference.
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{spare}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
}

// --- exam questions + answers (taking an exam) -----------------------------

/// Create a question tagged with `subject` via `POST /exams/{exam}/questions`
/// (asserts 201); returns its id.
async fn create_question(
    app: &axum::Router,
    cookie: &str,
    exam: &str,
    subject: &str,
    body: serde_json::Value,
) -> String {
    id_of(&create_question_body(app, cookie, exam, subject, body).await)
}

/// Like [`create_question`], but hands back the whole response: choice ids are
/// minted by the server, so a test that answers a question — or pictures one of
/// its options — has to read them off the response rather than count positions.
async fn create_question_body(
    app: &axum::Router,
    cookie: &str,
    exam: &str,
    subject: &str,
    mut body: serde_json::Value,
) -> serde_json::Value {
    body["subject_id"] = json!(subject);
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
    res.body
}

/// The minted id of the option at `index` of a question (or bank template)
/// response — what these tests used to write as a bare choice index.
fn choice_of(question: &serde_json::Value, index: usize) -> String {
    question["choices"][index]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("choice {index} of {question}"))
        .to_string()
}

/// A course with an open sync window, one subject, and one enrolled student —
/// the spine of the question/answer tests. Returns (course, exam, subject).
async fn open_exam_with_student(
    app: &axum::Router,
    teacher: &str,
    student_id: &str,
    course_title: &str,
) -> (String, String, String) {
    let course = create_course(app, teacher, course_title).await;
    let subject = create_subject(app, teacher, &course, "general").await;
    enroll(app, teacher, &course, student_id).await;
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        app,
        teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    (course, exam, subject)
}

#[tokio::test]
async fn question_crud_validation_and_rbac() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "q_t", "teacher").await;
    let other_teacher = login_as(&app, &db, "q_t2", "teacher").await;
    let student = login(&app, "quinn").await;
    let course = create_course(&app, &teacher, "logic").await;
    let subject = create_subject(&app, &teacher, &course, "propositions").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;

    // A choice question echoes its full authoring view, correct included.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(
            json!({ "subject_id": subject, "text": "2 + 2?", "kind": "choice", "points": 10,
                     "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}, {"id": "c2", "text": "5"}], "correct": "c1" }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["subject"], subject);
    assert_eq!(res.body["kind"], "choice");
    assert_eq!(res.body["points"], 10);
    assert_eq!(res.body["choices"][1]["text"], "4");
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);
    let choice_q = id_of(&res.body);

    // A text question carries neither choices nor correct.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "Explain.", "kind": "text", "points": 20 })),
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
    let questions = common::items(&res.body);
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
            json!({ "text": "x", "kind": "choice", "points": 1, "correct": "c0" }),
        ),
        (
            "choice without correct",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}] }),
        ),
        (
            "correct names no submitted choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c2" }),
        ),
        (
            "correct is blank",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "" }),
        ),
        (
            // Two options under one key: the payload can't say which one
            // `correct` (or a picture) means, so it is refused outright.
            "duplicate choice id in one payload",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c0", "text": "b"}], "correct": "c0" }),
        ),
        (
            // An option with no key cannot be named by `correct`.
            "correct names an unkeyed option",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"text": "a"}, {"text": "b"}], "correct": "c0" }),
        ),
        (
            "one choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}], "correct": "c0" }),
        ),
        (
            "eleven choices",
            json!({ "text": "x", "kind": "choice", "points": 1,
                                   "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}, {"id": "c2", "text": "c"}, {"id": "c3", "text": "d"}, {"id": "c4", "text": "e"}, {"id": "c5", "text": "f"}, {"id": "c6", "text": "g"}, {"id": "c7", "text": "h"}, {"id": "c8", "text": "i"}, {"id": "c9", "text": "j"}, {"id": "c10", "text": "k"}], "correct": "c0" }),
        ),
        (
            "blank choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": " "}], "correct": "c0" }),
        ),
        (
            "text with choices",
            json!({ "text": "x", "kind": "text", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}] }),
        ),
        (
            "text with correct",
            json!({ "text": "x", "kind": "text", "points": 1, "correct": "c0" }),
        ),
    ] {
        let mut body = body;
        body["subject_id"] = json!(subject);
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

    // The subject must exist and belong to the exam's own course.
    let other_course = create_course(&app, &teacher, "rhetoric").await;
    let foreign_subject = create_subject(&app, &teacher, &other_course, "fallacies").await;
    for (label, subject_id) in [
        ("unknown subject", "missing"),
        ("foreign subject", foreign_subject.as_str()),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions"),
            Some(&teacher),
            Some(json!({ "subject_id": subject_id, "text": "x", "kind": "text", "points": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    // Authoring needs management rights over the course; reading needs teacher+.
    let question_body = json!({ "subject_id": subject, "text": "x", "kind": "text", "points": 1 });
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
    assert_eq!(
        res.body["correct"], res.body["choices"][1]["id"],
        "kind bundle kept"
    );
    assert_eq!(res.body["subject"], subject, "subject kept by omission");

    // Re-tagging stays inside the course: another of its subjects is fine, a
    // foreign course's subject is a 400.
    let second_subject = create_subject(&app, &teacher, &course, "predicates").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "subject_id": second_subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], second_subject);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "subject_id": foreign_subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

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
        Some(json!({ "kind": "choice", "choices": [{"id": "c0", "text": "yes"}, {"id": "c1", "text": "no"}], "correct": "c0" })),
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
    let other_exam = create_exam(&app, &teacher, &course, "quiz", "quiz").await;
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
            "POST" => {
                Some(json!({ "subject_id": subject, "text": "x", "kind": "text", "points": 1 }))
            }
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
    assert_eq!(common::items(&res.body).len(), 1);
}

#[tokio::test]
async fn questions_freeze_once_attempts_start() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "frz_t", "teacher").await;
    let student = login(&app, "firat").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam, subject) =
        open_exam_with_student(&app, &teacher, &student_id, "algo").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);

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
        Some(json!({ "subject_id": subject, "text": "late", "kind": "text", "points": 1 })),
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
    let (_course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "art").await;
    let choice_q_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let choice_q = id_of(&choice_q_body);
    create_question(
        &app,
        &teacher,
        &exam,
        &subject,
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
    assert_eq!(questions[0]["choices"][1]["text"], "4");
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
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 0) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["question"], choice_q);
    assert_eq!(res.body["selected"], choice_of(&choice_q_body, 0));
    let saved_at = res.body["updated_at"].as_i64().expect("updated_at");

    let res = send(&app, "GET", &uri, Some(&student), None).await;
    let questions = res.body.as_array().expect("questions");
    assert_eq!(
        questions[0]["answer"]["selected"],
        choice_of(&choice_q_body, 0)
    );
    assert_eq!(questions[0]["answer"]["updated_at"], saved_at);
    assert!(questions[1]["answer"].is_null());
}

#[tokio::test]
async fn answer_saves_gate_on_attempt_state_and_kind() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ans_t", "teacher").await;
    let student = login(&app, "arda").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "phys").await;
    let choice_q_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}, {"id": "c2", "text": "5"}], "correct": "c1" }),
    )
    .await;
    let choice_q = id_of(&choice_q_body);
    let text_q_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;
    let text_q = id_of(&text_q_body);
    let answers_uri = format!("/exams/{exam}/attempt/answers");

    // Saving needs an attempt (404 before start).
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 0) })),
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
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 0) })),
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
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 2) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["selected"], choice_of(&choice_q_body, 2));
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
            json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 1), "text": "4" }),
        ),
        (
            "selected names no choice of this question",
            json!({ "question_id": choice_q, "selected": "01NOTACHOICEOFTHISQUEST" }),
        ),
        ("nothing to save", json!({ "question_id": choice_q })),
        (
            "selected on a text question",
            json!({ "question_id": text_q, "selected": choice_of(&choice_q_body, 0) }),
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
        Some(json!({ "question_id": "missing", "selected": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let foreign_exam = create_exam(&app, &teacher, &course, "other", "quiz").await;
    let foreign_q_body = create_question_body(
        &app,
        &teacher,
        &foreign_exam,
        &subject,
        json!({ "text": "not yours", "kind": "choice", "points": 1,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let foreign_q = id_of(&foreign_q_body);
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": foreign_q, "selected": choice_of(&foreign_q_body, 0) })),
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
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 1) })),
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
    assert_eq!(
        res.body[0]["answer"]["selected"],
        choice_of(&choice_q_body, 2)
    );
    assert_eq!(res.body[1]["answer"]["text"], "because");
}

#[tokio::test]
async fn unenrollment_cuts_the_sittings_reads_and_writes() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "cut_t", "teacher").await;
    let student = login(&app, "cansu").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "chem").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);

    // Enrolled: start the attempt, read the questions, save an answer.
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
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Unenrolled mid-exam: the course wall closes over the sitting too. The
    // question list is course content (`GET /exams/{id}` is already walled),
    // and answering from outside the course would dodge the same wall the
    // exam room enforces on entry.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{student_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
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
        StatusCode::FORBIDDEN,
        "question list after unenrollment: {}",
        res.body
    );
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 0) })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "answer save after unenrollment: {}",
        res.body
    );

    // Their own status and submission stay theirs: like the rejoin lock,
    // finishing submits what's already saved and writes nothing new.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "finish stays open: {}",
        res.body
    );
    assert_eq!(res.body["status"], "submitted");
    assert_eq!(res.body["answered"], 1, "the enrolled-time answer survives");
}

#[tokio::test]
async fn answer_saves_stop_at_the_deadline() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "dl_t", "teacher").await;
    let student = login(&app, "dila").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "geo").await;
    let subject = create_subject(&app, &teacher, &course, "general").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // A window that closes right after the start — real time, the server
    // judges expiry by `Timestamp::now()`.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "blitz", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 1_500 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "quick", "kind": "choice", "points": 1,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
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
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 0) })),
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
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
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
    assert_eq!(
        res.body["answers"][0]["selected"],
        choice_of(&question_body, 0)
    );
}

#[tokio::test]
async fn teacher_answer_sheet_judges_choices_and_suggests_a_score() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sc_t", "teacher").await;
    let student = login(&app, "sena").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam, subject) =
        open_exam_with_student(&app, &teacher, &student_id, "chem").await;
    let q1_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let q1 = id_of(&q1_body);
    let q2_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "3 + 3?", "kind": "choice", "points": 20,
                "choices": [{"id": "c0", "text": "6"}, {"id": "c1", "text": "7"}], "correct": "c0" }),
    )
    .await;
    let q2 = id_of(&q2_body);
    let q3 = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
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
        json!({ "question_id": q1, "selected": choice_of(&q1_body, 1) }), // right
        json!({ "question_id": q2, "selected": choice_of(&q2_body, 1) }), // wrong
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
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &ayla_id, "stats").await;
    enroll(&app, &teacher, &course, &bora_id).await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);
    create_question(
        &app,
        &teacher,
        &exam,
        &subject,
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
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
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
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "geo2").await;
    let q1_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let q1 = id_of(&q1_body);
    let q2_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;
    let q2 = id_of(&q2_body);
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
        json!({ "question_id": q1, "selected": choice_of(&q1_body, 1) }),
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
        json!({ "title": "retake", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let q_body = create_question_body(
        &app,
        &teacher,
        &exam2,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let q = id_of(&q_body);
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
        Some(json!({ "question_id": q, "selected": choice_of(&q_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    unenroll(&app, &teacher, &course, &student_id).await;
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
    // The course's subjects go down with it too.
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "subject cascaded");
}

#[tokio::test]
async fn question_patch_revalidates_the_stale_kind_bundle() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rv_t", "teacher").await;
    let course = create_course(&app, &teacher, "sets").await;
    let subject = create_subject(&app, &teacher, &course, "unions").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}, {"id": "c2", "text": "c"}], "correct": "c2" }),
    )
    .await;
    let question = id_of(&question_body);
    let uri = format!("/exams/{exam}/questions/{question}");

    // Shrinking the options while keeping the old `correct` would leave it
    // dangling past the end — the merge must re-validate the bundle.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}] })),
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
        Some(json!({ "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);

    // A lone out-of-range `correct` against the kept choices is caught too.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "correct": "c5" })),
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
    assert_eq!(
        common::items(&res.body)[0]["choices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        common::items(&res.body)[0]["correct"],
        common::items(&res.body)[0]["choices"][1]["id"]
    );
}

#[tokio::test]
async fn question_authoring_follows_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "own_t", "teacher").await;
    let boss = login_as(&app, &db, "own_m", "manager").await;
    let course = create_course(&app, &teacher, "greek").await;
    let subject = create_subject(&app, &boss, &course, "alphabet").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;

    // A manager+ authors questions in anyone's course, like every other
    // course-management write.
    let question_body = create_question_body(
        &app,
        &boss,
        &exam,
        &subject,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
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
    let (_course, exam, subject) =
        open_exam_with_student(&app, &teacher, &student_id, "race").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}, {"id": "c2", "text": "c"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
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
        let body = json!({ "question_id": question, "selected": choice_of(&question_body, (i % 3) as usize) });
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

    // Reads follow course visibility: an unenrolled student is refused …
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/sessions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // … an enrolled one reads; parents must exist.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/enrollments"),
        Some(&owner),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
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
    assert_eq!(common::items(&res.body).len(), 3);
    assert_eq!(common::items(&res.body)[2]["starts_at"], now + 120_000);
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
    // The session's own teacher reads it without course rights; a session of
    // someone else's course stays out of reach.
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{rival_session}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
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
    // The roster follows roll-call rights: the session teacher reads it, a
    // student (even a listed one) and an unrelated teacher do not — students
    // get their own tallies via `/attendance/me`.
    let res = send(&app, "GET", &mark_uri, Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let roster = common::items(&res.body);
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["status"], "absent");
    let res = send(&app, "GET", &mark_uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", &mark_uri, Some(&rival), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

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
    unenroll(&app, &owner, &course, &ali_id).await;
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
    let log = common::items(&res.body);
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
    let log = common::items(&res.body);
    assert_eq!(log.len(), 2);
    assert!(log[0]["check_out"].is_null(), "open stint sorts newest");
    assert!(!log[1]["check_out"].is_null());
}

// --- pomodoro ----------------------------------------------------------------

#[tokio::test]
async fn pomodoro_restart_replaces_finish_closes_and_teachers_read() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali_id = me_id(&app, &ali).await;

    // Students only on start; finishing with nothing running conflicts.
    let res = send(&app, "POST", "/pomodoro/start", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "POST", "/pomodoro/finish", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = send(&app, "POST", "/pomodoro/start", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["finished_at"].is_null());
    let first_start = res.body["started_at"].as_i64().unwrap();

    // A restart discards the dangling session: one row, clock reset allowed.
    let res = send(&app, "POST", "/pomodoro/start", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["started_at"].as_i64().unwrap() >= first_start);
    let res = send(&app, "GET", "/pomodoro/me", Some(&ali), None).await;
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(
        res.body["total_focus_ms"], 0,
        "running session counts nothing"
    );

    let res = send(&app, "POST", "/pomodoro/finish", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let finished_at = res.body["finished_at"].as_i64().unwrap();
    let duration = res.body["duration_ms"].as_i64().unwrap();
    assert_eq!(
        duration,
        finished_at - res.body["started_at"].as_i64().unwrap()
    );
    let res = send(&app, "POST", "/pomodoro/finish", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "already finished");

    // The open slot is free again; the log keeps both sessions, newest first,
    // and the focus total sums only the finished one.
    let res = send(&app, "POST", "/pomodoro/start", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(&app, "GET", "/pomodoro/me", Some(&ali), None).await;
    let log = common::items(&res.body);
    assert_eq!(log.len(), 2);
    assert!(log[0]["finished_at"].is_null(), "open session sorts newest");
    assert_eq!(res.body["total_focus_ms"].as_i64().unwrap(), duration);

    // Reading someone's log is teacher+; unknown users are 404.
    let res = send(
        &app,
        "GET",
        &format!("/pomodoro/{ali_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/pomodoro/{ali_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body).len(), 2);
    let res = send(&app, "GET", "/pomodoro/01UNKNOWN", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
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
    assert_eq!(common::items(&res.body).len(), 2);
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

#[tokio::test]
async fn work_duration_survives_absurd_corrected_instants() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "wid_t", "teacher").await;
    let boss = login_as(&app, &db, "wid_m", "manager").await;
    send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    let closed = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    let closed_id = id_of(&closed.body);

    // Corrected instants are arbitrary i64 millis; the widest legal pair must
    // not overflow the `check_out - check_in` duration (a wrapped negative in
    // release, a panicking 500 in debug) — it saturates instead.
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&boss),
        Some(json!({ "check_in": i64::MIN, "check_out": i64::MAX })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["duration_ms"].as_i64(),
        Some(i64::MAX),
        "the over-wide stint saturates: {}",
        res.body
    );
    // The stored row must stay readable, not a permanent 500.
    let res = send(&app, "GET", "/work/me", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
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

    // Two event rows, both teacher-marked (students never mark).
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
        Some(&owner),
        Some(json!({ "status": "present", "user_id": ali_id })),
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

    // A teacher's view narrows to the courses they manage: rival runs only
    // chemistry, so ali's report shows that block alone — the overall session
    // tally follows, while event tallies stay school-wide.
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let chemistry = create_course(&app, &rival, "chemistry").await;
    enroll(&app, &rival, &chemistry, &ali_id).await;
    let session = create_session(
        &app,
        &rival,
        &chemistry,
        Timestamp::now().as_millis() + 10 * 3_600_000,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&rival),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["events"]["total"], 2, "events stay school-wide");
    assert_eq!(res.body["sessions"]["total"], 1);
    let blocks = res.body["courses"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["course"]["title"], "chemistry");

    // The owner's view still carries only their own two courses …
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.body["courses"].as_array().unwrap().len(), 2);
    assert_eq!(res.body["sessions"]["total"], 4);

    // … and the self view stays complete: all three courses, every row.
    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.body["courses"].as_array().unwrap().len(), 3);
    assert_eq!(res.body["sessions"]["total"], 5);

    let res = send(&app, "GET", "/attendance/01UNKNOWN", Some(&owner), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

// --- users: search matching semantics --------------------------------------

/// The search fragment is matched with literal `CONTAINS` semantics — never
/// SQL-`LIKE`. A regression to a `LIKE '%{q}%'` pattern would turn `%` and `_`
/// in the query into wildcards: `q=%` would dump every named user and `q=_`
/// every username, defeating the "real substring only" intent of the picker.
/// `%` and `_` must match only users who literally carry those characters.
#[tokio::test]
async fn search_matches_are_literal_not_like_wildcards() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;

    // Three students: one plainly named, one with a literal `%` in their name,
    // one with a literal `_` in their username.
    let alice = login(&app, "alice").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&alice),
        Some(json!({ "name": "Plain", "surname": "Jane" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let percy = login(&app, "percy").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&percy),
        Some(json!({ "name": "100% legend" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    login(&app, "under_score").await;

    // `q=%` (url-encoded %25) is a literal percent sign: only percy matches.
    // Under LIKE semantics it would match every user with a name.
    let res = send(&app, "GET", "/users/search?q=%25", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let hits = common::items(&res.body);
    assert_eq!(hits.len(), 1, "`%` must match literally: {hits:?}");
    assert_eq!(hits[0]["username"], "percy");

    // `q=_` is a literal underscore: only under_score matches. Under LIKE
    // semantics `_` is any-single-character and would match everyone.
    let res = send(&app, "GET", "/users/search?q=_", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let hits = common::items(&res.body);
    assert_eq!(hits.len(), 1, "`_` must match literally: {hits:?}");
    assert_eq!(hits[0]["username"], "under_score");
}

/// A blank search fragment is refused, not treated as match-everything.
#[tokio::test]
async fn search_rejects_a_blank_query() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    login(&app, "alice").await;

    // Whitespace-only (url-encoded spaces) -> 400, not the full user list.
    let res = send(&app, "GET", "/users/search?q=%20%20", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Missing `q` entirely is a deserialization failure, not a wildcard.
    let res = send(&app, "GET", "/users/search", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

/// The app is EN/TR, so the user picker must fold Turkish casing: `İ` (U+0130)
/// lowercases to `i` + U+0307 in both Rust and SurrealQL, which used to make
/// `ilker` and `İLKER` two disjoint searches — a teacher typing lowercase got
/// an empty picker. Same defect, same fix as the bank-question search.
#[tokio::test]
async fn user_search_folds_turkish_casing() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;

    let ilker = login(&app, "ilker_a").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ilker),
        Some(json!({ "name": "İLKER", "surname": "ÇAĞLAR" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let ilknur = login(&app, "ilknur_b").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ilknur),
        Some(json!({ "name": "ilknur", "surname": "Işık" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Every casing of the shared `ilk` prefix finds both, in both directions:
    // `ilk`, `İLK`, `ılk`, `ILK`.
    for needle in ["ilk", "%C4%B0LK", "%C4%B1lk", "ILK"] {
        let res = send(
            &app,
            "GET",
            &format!("/users/search?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let hits = common::items(&res.body);
        assert_eq!(hits.len(), 2, "needle {needle} missed: {hits:?}");
    }

    // Diacritics fold on the surname too, both ways (`ÇAĞLAR` ↔ `caglar`,
    // `Işık` ↔ `isik`).
    for (needle, username) in [
        ("caglar", "ilker_a"),
        ("%C3%A7a%C4%9Flar", "ilker_a"),
        ("isik", "ilknur_b"),
        ("I%C5%9F%C4%B1k", "ilknur_b"),
    ] {
        let res = send(
            &app,
            "GET",
            &format!("/users/search?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        let hits = common::items(&res.body);
        assert_eq!(hits.len(), 1, "needle {needle} missed: {hits:?}");
        assert_eq!(hits[0]["username"], username);
    }

    // Folding widens the match, it does not match everything.
    let res = send(&app, "GET", "/users/search?q=zeynep", Some(&teacher), None).await;
    assert_eq!(common::items(&res.body).len(), 0);
}

// --- courses: catalog dedup -------------------------------------------------

/// A user who both created a course (while staff) and is enrolled in it (after
/// a demotion to student) sees it once in the catalog, not twice — the created
/// and enrolled sources are deduplicated. Enrollment is student-only now, so
/// that overlap can only arise across a role change; the dedup must still hold.
#[tokio::test]
async fn course_catalog_lists_a_creator_enrolled_course_once() {
    let (app, db) = app_and_db().await;
    let creator = login_as(&app, &db, "teacher", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let creator_id = me_id(&app, &creator).await;

    let course = create_course(&app, &creator, "algebra").await;
    // Author the exam while still staff, before the demotion below.
    let exam = create_exam(&app, &creator, &course, "mt", "quiz").await;

    // Demote the creator to student, then have a manager enroll them into their
    // own course: they now show up in both the created and enrolled sources.
    set_role(&db, "teacher", "student").await;
    enroll(&app, &boss, &course, &creator_id).await;

    let res = send(&app, "GET", "/courses", Some(&creator), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let courses = common::items(&res.body);
    assert_eq!(courses.len(), 1, "created+enrolled must dedup: {courses:?}");
    assert_eq!(courses[0]["id"], json!(course));

    // The exam catalog derives from the same visible set and must not double
    // the course's exams either.
    let res = send(&app, "GET", "/exams", Some(&creator), None).await;
    let exams = common::items(&res.body);
    assert_eq!(exams.len(), 1, "one exam listed once: {exams:?}");
    assert_eq!(exams[0]["id"], json!(exam));
}

// --- school settings + terms ----------------------------------------------

#[tokio::test]
async fn settings_serve_defaults_and_gate_edits_to_manager() {
    let (app, db) = app_and_db().await;
    let student = login(&app, "set.student").await;
    let teacher = login_as(&app, &db, "set.teacher", "teacher").await;
    let manager = login_as(&app, &db, "set.manager", "manager").await;

    // Reading requires a session; the policy is not public.
    let res = send(&app, "GET", "/settings", None, None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    // Any authenticated user reads; the defaults mirror the old constants,
    // every kind weighing 1 until the school says otherwise.
    let res = send(&app, "GET", "/settings", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            {"name": "homework", "weight": 1},
            {"name": "quiz", "weight": 1},
            {"name": "midterm", "weight": 1},
            {"name": "final", "weight": 1},
            {"name": "project", "weight": 1},
            {"name": "oral", "weight": 1},
        ])
    );
    assert_eq!(
        res.body["attendance_statuses"],
        json!(["present", "absent", "late", "excused"])
    );
    assert_eq!(res.body["grade_bands"], json!([]));

    // Students and teachers cannot edit school policy.
    for cookie in [&student, &teacher] {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(cookie),
            Some(json!({ "exam_kinds": [{"name": "lab", "weight": 1}] })),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
    }

    // A manager edits one field; omitted fields keep their value.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "lab", "weight": 2},
            {"name": "Quiz", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            {"name": "lab", "weight": 2},
            {"name": "Quiz", "weight": 1},
        ])
    );
    assert_eq!(
        res.body["attendance_statuses"],
        json!(["present", "absent", "late", "excused"])
    );

    // Every reader sees the new policy at once.
    let res = send(&app, "GET", "/settings", Some(&student), None).await;
    assert_eq!(res.body["exam_kinds"][0]["name"], "lab");
    assert_eq!(res.body["exam_kinds"][0]["weight"], 2);
}

#[tokio::test]
async fn settings_validation_rejects_bad_lists_and_bands() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "val.manager", "manager").await;

    let bad = [
        json!({ "exam_kinds": [] }), // a school needs at least one kind
        json!({ "exam_kinds": [{"name": "   ", "weight": 1}] }), // blank name
        // Case-insensitive duplicate names.
        json!({ "exam_kinds": [
            {"name": "Lab", "weight": 1},
            {"name": "lab", "weight": 2},
        ]}),
        // Weights are held to 1–100.
        json!({ "exam_kinds": [{"name": "quiz", "weight": 0}] }),
        json!({ "exam_kinds": [{"name": "quiz", "weight": 101}] }),
        json!({ "exam_kinds": [{"name": "quiz", "weight": -1}] }),
        json!({ "attendance_statuses": ["present", "absent", "late"] }), // core dropped
        json!({ "grade_bands": [{ "min": 50, "label": "CC" }] }),        // no band starts at 0
        json!({ "grade_bands": [{ "min": 0, "label": "F" }, { "min": 0, "label": "E" }] }),
        json!({ "grade_bands": [{ "min": -1, "label": "F" }] }),
        json!({ "grade_bands": [{ "min": 101, "label": "A" }] }),
        json!({ "grade_bands": [{ "min": 0, "label": "  " }] }),
    ];
    for body in bad {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(&manager),
            Some(body.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // No failed PATCH half-applied anything.
    let res = send(&app, "GET", "/settings", Some(&manager), None).await;
    assert_eq!(res.body["exam_kinds"].as_array().unwrap().len(), 6);
    assert_eq!(res.body["grade_bands"], json!([]));
}

#[tokio::test]
async fn exam_kinds_follow_settings_for_new_writes_only() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "kind.teacher", "teacher").await;
    let manager = login_as(&app, &db, "kind.manager", "manager").await;

    let course = create_course(&app, &teacher, "Chemistry").await;
    let exam = create_exam(&app, &teacher, &course, "Midterm", "midterm").await;

    // The school swaps its kind list wholesale.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{"name": "lab", "weight": 1}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // New exams are held to the new list.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Retired kind", "kind": "midterm" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Lab 1", "kind": "lab" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // The stored exam keeps its retired kind: a title-only PATCH passes…
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "title": "Midterm A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["kind"], "midterm");
    // …but a kind this request sets is validated against the current list.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "kind": "midterm" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "kind": "lab" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["kind"], "lab");
}

#[tokio::test]
async fn attendance_statuses_follow_settings_and_bucket_in_reports() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "att.manager", "manager").await;
    let student = login(&app, "att.student").await;
    let student_id = me_id(&app, &student).await;

    // Add a school-specific status on top of the mandatory core.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "attendance_statuses": ["present", "absent", "late", "excused", "online"]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&manager),
        Some(json!({ "title": "Assembly" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let event = id_of(&res.body);

    // The custom status is markable; garbage still is not.
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&manager),
        Some(json!({ "status": "online", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&manager),
        Some(json!({ "status": "maybe", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // The report buckets it under `custom` and keeps it out of the rate.
    let res = send(&app, "GET", "/attendance/me", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["events"]["custom"]["online"], 1);
    assert_eq!(res.body["events"]["total"], 1);
    assert_eq!(res.body["events"]["rate"], serde_json::Value::Null);

    // Retiring the status blocks new marks; the stored row keeps reporting.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "attendance_statuses": ["present", "absent", "late", "excused"]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&manager),
        Some(json!({ "status": "online", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(&app, "GET", "/attendance/me", Some(&student), None).await;
    assert_eq!(res.body["events"]["custom"]["online"], 1);
}

#[tokio::test]
async fn grade_bands_label_the_marks_report() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "band.teacher", "teacher").await;
    let manager = login_as(&app, &db, "band.manager", "manager").await;
    let student = login(&app, "band.student").await;
    let student_id = me_id(&app, &student).await;

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "grade_bands": [
                { "min": 0, "label": "FF" },
                { "min": 50, "label": "CC" },
                { "min": 85, "label": "AA" },
            ]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // Bands echo highest-first — the canonical order.
    assert_eq!(res.body["grade_bands"][0]["label"], "AA");

    let course = create_course(&app, &teacher, "Physics").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = create_exam(&app, &teacher, &course, "Final", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "user_id": student_id, "mark": 90 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(&app, "GET", "/marks/me", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let block = &res.body["courses"][0];
    assert_eq!(block["results"][0]["mark"], 90);
    assert_eq!(block["results"][0]["grade"], "AA");
    assert_eq!(block["average"], 90.0);
    assert_eq!(block["average_grade"], "AA");
    assert_eq!(res.body["overall_grade"], "AA");

    // Clearing the bands turns labels off without touching the numbers.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "grade_bands": [] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(&app, "GET", "/marks/me", Some(&student), None).await;
    assert_eq!(res.body["courses"][0]["results"][0]["mark"], 90);
    assert_eq!(
        res.body["courses"][0]["results"][0]["grade"],
        serde_json::Value::Null
    );
    assert_eq!(res.body["overall_grade"], serde_json::Value::Null);
}

#[tokio::test]
async fn terms_crud_gates_and_links_to_courses() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "term.manager", "manager").await;
    let teacher = login_as(&app, &db, "term.teacher", "teacher").await;
    let student = login(&app, "term.student").await;

    // Writes are manager+; and a term may lie fully in the past — a school
    // adopting the app mid-year backfills its calendar, unlike the no-past
    // rule on exams/lessons/events.
    let past_term = json!({
        "name": "2025 Fall",
        "starts_at": 1_600_000_000_000_i64,
        "ends_at": 1_610_000_000_000_i64,
    });
    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&teacher),
        Some(past_term.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "POST", "/terms", Some(&manager), Some(past_term)).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let term = id_of(&res.body);

    // The range stays ordered, merged PATCHes included.
    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "Broken", "starts_at": 2, "ends_at": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/terms/{term}"),
        Some(&manager),
        Some(json!({ "ends_at": 1_500_000_000_000_i64 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/terms/{term}"),
        Some(&manager),
        Some(json!({ "name": "2025/26 Fall" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "2025/26 Fall");

    // Any authenticated user reads the calendar.
    let res = send(&app, "GET", "/terms", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body).len(), 1);

    // Courses link to a term at creation; a bogus id is a 400, not a silent null.
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "History", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let course = id_of(&res.body);
    assert_eq!(res.body["term"].as_str(), Some(term.as_str()));
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "Broken", "term_id": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // PATCH: null unlinks, a value re-links, omitting keeps.
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "term_id": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["term"], serde_json::Value::Null);
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "term_id": term })),
    )
    .await;
    assert_eq!(res.body["term"].as_str(), Some(term.as_str()));
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "title": "History II" })),
    )
    .await;
    assert_eq!(res.body["term"].as_str(), Some(term.as_str()));

    // A term a course still points at cannot be deleted; unlinking the course
    // clears the way, and the course itself survives untouched.
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{term}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "term_id": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{term}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &format!("/terms/{term}"), Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "History II");
    assert_eq!(res.body["term"], serde_json::Value::Null);
}

#[tokio::test]
async fn roll_call_accepts_school_statuses_and_buckets_session_reports() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "roll.manager", "manager").await;
    let teacher = login_as(&app, &db, "roll.teacher", "teacher").await;
    let student = login(&app, "roll.student").await;
    let student_id = me_id(&app, &student).await;

    // The event path is already covered; this proves the second marking
    // path — lesson roll call — reads the same settings list.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "attendance_statuses": ["present", "absent", "late", "excused", "online"]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let course = create_course(&app, &teacher, "Biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let future = Timestamp::now().as_millis() + 3_600_000;
    let session = create_session(&app, &teacher, &course, future).await;

    // Custom status marks; garbage still dies.
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&teacher),
        Some(json!({ "status": "online", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "online");
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&teacher),
        Some(json!({ "status": "onsite", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Both the overall session tally and the per-course block bucket it
    // under `custom`, rate-neutral.
    let res = send(&app, "GET", "/attendance/me", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["sessions"]["custom"]["online"], 1);
    assert_eq!(res.body["sessions"]["total"], 1);
    assert_eq!(res.body["sessions"]["rate"], serde_json::Value::Null);
    let block = &res.body["courses"][0];
    assert_eq!(block["counts"]["custom"]["online"], 1);
    assert_eq!(block["counts"]["rate"], serde_json::Value::Null);
}

#[tokio::test]
async fn terms_list_newest_first_require_auth_and_guard_delete_per_term() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "bulk.manager", "manager").await;
    let teacher = login_as(&app, &db, "bulk.teacher", "teacher").await;

    // The calendar is not public.
    let res = send(&app, "GET", "/terms", None, None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    let older = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "2025 Fall", "starts_at": 1_000, "ends_at": 2_000 })),
    )
    .await;
    let newer = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "2026 Spring", "starts_at": 5_000, "ends_at": 6_000 })),
    )
    .await;
    let (older, newer) = (id_of(&older.body), id_of(&newer.body));

    // Newest (by starts_at) first.
    let res = send(&app, "GET", "/terms", Some(&teacher), None).await;
    let names: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["2026 Spring", "2025 Fall"]);

    // The delete guard counts EVERY linked course — unlinking one of two is
    // not enough — and it is per term: a course on another term never blocks.
    let mut linked = Vec::new();
    for title in ["Algebra", "Geometry"] {
        let res = send(
            &app,
            "POST",
            "/courses",
            Some(&teacher),
            Some(json!({ "title": title, "term_id": newer })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        linked.push(id_of(&res.body));
    }
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "History", "term_id": older })),
    )
    .await;
    let unrelated = id_of(&res.body);

    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{newer}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    for (i, course) in linked.iter().enumerate() {
        let res = send(
            &app,
            "PATCH",
            &format!("/courses/{course}"),
            Some(&teacher),
            Some(json!({ "term_id": null })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
        // Still blocked until the LAST linked course lets go.
        let res = send(
            &app,
            "DELETE",
            &format!("/terms/{newer}"),
            Some(&manager),
            None,
        )
        .await;
        let expected = if i + 1 == linked.len() {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(res.status, expected, "after unlinking {course}");
    }
    let res = send(
        &app,
        "GET",
        &format!("/courses/{unrelated}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["term"].as_str(), Some(older.as_str()));
}

#[tokio::test]
async fn grade_band_boundary_applies_to_the_weighted_average() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "edge.teacher", "teacher").await;
    let manager = login_as(&app, &db, "edge.manager", "manager").await;
    let student = login(&app, "edge.student").await;
    let student_id = me_id(&app, &student).await;

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "grade_bands": [{ "min": 0, "label": "FF" }, { "min": 85, "label": "AA" }]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Two equal-weight marks straddling the band edge: 80 and 90 → average
    // exactly 85.0, which is INSIDE the AA band (min is inclusive), while
    // the 80 itself still reads FF.
    let course = create_course(&app, &teacher, "Calculus").await;
    enroll(&app, &teacher, &course, &student_id).await;
    for (title, mark) in [("Quiz A", 80), ("Quiz B", 90)] {
        let exam = create_exam(&app, &teacher, &course, title, "quiz").await;
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "user_id": student_id, "mark": mark })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    let res = send(&app, "GET", "/marks/me", Some(&student), None).await;
    let block = &res.body["courses"][0];
    assert_eq!(block["average"], 85.0);
    assert_eq!(block["average_grade"], "AA");
    let grades: Vec<(&str, &str)> = block["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["title"].as_str().unwrap(), r["grade"].as_str().unwrap()))
        .collect();
    assert!(grades.contains(&("Quiz A", "FF")));
    assert!(grades.contains(&("Quiz B", "AA")));
    assert_eq!(res.body["overall_grade"], "AA");
}

/// Two managers patch DIFFERENT fields concurrently. Neither edit may be lost
/// and neither request may 500 — the compare-and-set loop absorbs the
/// conflict and re-merges (the guard `save_if_unchanged` exists for).
#[tokio::test]
async fn concurrent_settings_patches_both_land() {
    for _round in 0..10 {
        let (app, db) = app_and_db().await;
        let mavi = login_as(&app, &db, "mavi", "manager").await;
        let kara = login_as(&app, &db, "kara", "manager").await;

        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                "/settings",
                Some(&mavi),
                Some(
                    json!({ "grade_bands": [ { "min": 0, "label": "F" }, { "min": 50, "label": "P" } ] })
                ),
            ),
            send(
                &app,
                "PATCH",
                "/settings",
                Some(&kara),
                Some(json!({ "max_file_bytes": 4096 })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "bands patch: {}", a.body);
        assert_eq!(b.status, StatusCode::OK, "bytes patch: {}", b.body);

        let after = send(&app, "GET", "/settings", Some(&mavi), None).await;
        assert_eq!(after.body["max_file_bytes"], 4096, "{}", after.body);
        assert_eq!(
            after.body["grade_bands"].as_array().unwrap().len(),
            2,
            "both concurrent edits must survive: {}",
            after.body
        );
    }
}

#[tokio::test]
async fn settings_accept_admin_edits_trim_entries_and_noop_on_empty_patch() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "adm.admin", "admin").await;

    // An empty PATCH is a valid no-op: current policy back, nothing changed.
    let res = send(&app, "PATCH", "/settings", Some(&admin), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["exam_kinds"].as_array().unwrap().len(), 6);

    // Admin clears the manager bar (hierarchy, not equality), and kind names
    // arrive trimmed on the wire.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "exam_kinds": [
            {"name": "  lab  ", "weight": 2},
            {"name": "quiz", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            {"name": "lab", "weight": 2},
            {"name": "quiz", "weight": 1},
        ])
    );
}

// --- question images ---------------------------------------------------------

/// POST `bytes` as a multipart image upload to `path`; returns (status, json).
async fn post_image(
    app: &axum::Router,
    cookie: &str,
    path: &str,
    content_type: &str,
    bytes: &[u8],
) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = common::send_raw(
        app,
        "POST",
        path,
        Some(cookie),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        common::multipart_file("pic.png", content_type, bytes),
    )
    .await;
    let body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

/// Every stored question-image blob name, straight from the table.
async fn image_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM question_image")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Every stored bank-question-image blob name, straight from the table.
async fn bank_image_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM bank_question_image")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Every stored answer-image (student drawing) blob name, straight from the table.
async fn answer_image_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM answer_image")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// The full life of question images: teacher uploads (question + choice slots,
/// rasters only, choice slots bounded), metadata on both question views,
/// bytes behind the sitting wall, replace swaps the blob, a choices PATCH
/// drops the option pictures, and every delete path takes the blobs with it.
#[tokio::test]
async fn question_images_author_serve_and_cascade() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "img_t", "teacher").await;
    let student = login(&app, "img_s").await;
    let student_id = me_id(&app, &student).await;
    let outsider = login(&app, "img_o").await;

    let course = create_course(&app, &teacher, "geography").await;
    let subject = create_subject(&app, &teacher, &course, "maps").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "map quiz", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Which city is marked?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "Ankara"}, {"id": "c1", "text": "İzmir"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
    let essay = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Describe the marked region.", "kind": "text", "points": 10 }),
    )
    .await;

    let q_image = format!("/exams/{exam}/questions/{question}/image");
    let png = b"png-bytes-question".as_slice();

    // SVG is out (script risk); unknown types are out; rasters land.
    let (status, body) = post_image(&app, &teacher, &q_image, "image/svg+xml", b"<svg/>").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_image(&app, &teacher, &q_image, "application/pdf", b"%PDF").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_image(&app, &teacher, &q_image, "image/png", png).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["content_type"], "image/png");
    assert_eq!(body["size"], png.len() as i64);

    // A classical (text) question takes an illustration too — but no option
    // pictures, and no out-of-range slot anywhere.
    let (status, body) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{essay}/image"),
        "image/jpeg",
        b"jpeg-essay",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!(
            "/exams/{exam}/questions/{essay}/choices/{}/image",
            choice_of(&question_body, 0)
        ),
        "image/png",
        b"x",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "text questions take no option pictures"
    );
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{question}/choices/01NOTACHOICEOFTHISQUEST/image"),
        "image/png",
        b"x",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "slot must name a choice");

    // Option picture on choice 1, and the author view carries all the metas.
    let choice_image = format!(
        "/exams/{exam}/questions/{question}/choices/{}/image",
        choice_of(&question_body, 1)
    );
    let (status, _) = post_image(&app, &teacher, &choice_image, "image/webp", b"webp-choice").await;
    assert_eq!(status, StatusCode::CREATED);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let listed = &common::items(&res.body)[0];
    assert_eq!(listed["image"]["content_type"], "image/png");
    assert!(listed["choice_images"][0].is_null());
    assert_eq!(listed["choice_images"][1]["content_type"], "image/webp");

    // Replace swaps the blob under the same slot: one row, fresh bytes.
    let keys_before = image_blob_keys(&db).await;
    let png2 = b"png-bytes-question-v2".as_slice();
    let (status, body) = post_image(&app, &teacher, &q_image, "image/png", png2).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["size"], png2.len() as i64);
    let keys_after = image_blob_keys(&db).await;
    assert_eq!(keys_before.len(), keys_after.len(), "replace adds no row");
    for stale in keys_before.iter().filter(|k| !keys_after.contains(k)) {
        assert!(
            !common::files_dir().join(stale).exists(),
            "replaced blob lingers on disk"
        );
    }

    // The bytes sit behind the sitting wall: outsiders 403, enrolled students
    // 404 until an attempt exists, then 200 with the guard headers on.
    let (status, _, _) =
        common::send_raw(&app, "GET", &q_image, Some(&outsider), None, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) =
        common::send_raw(&app, "GET", &q_image, Some(&student), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no early peek before the attempt"
    );
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &q_image, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png2);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");
    let (status, _, bytes) =
        common::send_raw(&app, "GET", &choice_image, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"webp-choice");

    // The sitting view embeds the same metadata the author sees (sans key).
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body[0]["image"]["content_type"], "image/png");
    assert_eq!(
        res.body[0]["choice_images"][1]["content_type"],
        "image/webp"
    );
    assert_eq!(res.body[1]["image"]["content_type"], "image/jpeg");
    assert!(res.body[1]["choice_images"].is_null());

    // Images freeze with the rest of the question once attempts exist.
    let (status, body) = post_image(&app, &teacher, &q_image, "image/png", b"late").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let res = send(&app, "DELETE", &q_image, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // Deleting the exam takes every image row and blob with it.
    let keys = image_blob_keys(&db).await;
    assert_eq!(keys.len(), 3);
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
        image_blob_keys(&db).await.is_empty(),
        "rows survived the cascade"
    );
    for key in &keys {
        assert!(
            !common::files_dir().join(key).exists(),
            "blob {key} survived the exam delete"
        );
    }
}

/// The full life of a student's answer drawing: the student is the uploader
/// (rasters only), the bytes sit behind the sitting wall (own GET 404 before an
/// attempt, 200 after with the guard headers), the drawing meta rides both the
/// student's own sitting view and the teacher grading sheet, the teacher reads
/// the bytes via `/attempts/{user}/...` while an outsider cannot, a retake wipes
/// the row *and* its on-disk blob, and deleting the exam takes rows + blobs.
#[tokio::test]
async fn student_answer_images_serve_and_cascade() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ans_t", "teacher").await;
    let student = login(&app, "ans_s").await;
    let student_id = me_id(&app, &student).await;
    let outsider = login(&app, "ans_o").await;

    let course = create_course(&app, &teacher, "science").await;
    let subject = create_subject(&app, &teacher, &course, "biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    // Retakes allowed (max_attempts: 2) so the wipe path is reachable.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "cell quiz", "kind": "quiz", "mode": "open", "max_attempts": 2 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Draw a cell.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);

    let own = format!("/exams/{exam}/attempt/answers/{question}/image");
    let png = b"png-drawing".as_slice();

    // Before an attempt exists there is nothing to write into and no early
    // peek: the write path 404s (no attempt), the own-read 404s too.
    let (status, _) = post_image(&app, &student, &own, "image/png", png).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no attempt yet");
    let (status, _, _) =
        common::send_raw(&app, "GET", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no early peek before the attempt"
    );

    // The student sits the exam.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The raster wall holds on the student's own upload: SVG/PDF out, rasters in.
    let (status, body) = post_image(&app, &student, &own, "image/svg+xml", b"<svg/>").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_image(&app, &student, &own, "application/pdf", b"%PDF").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    for ct in ["image/png", "image/jpeg", "image/webp"] {
        let (status, body) = post_image(&app, &student, &own, ct, png).await;
        assert_eq!(status, StatusCode::CREATED, "{ct}: {body}");
        assert_eq!(body["content_type"], ct);
        assert_eq!(body["size"], png.len() as i64);
    }
    // Same (question, user) slot: three uploads collapse to one upserted row.
    assert_eq!(
        answer_image_blob_keys(&db).await.len(),
        1,
        "own slot upserts, adds no row"
    );

    // The student reads their own drawing back: 200, the guard headers, the
    // bytes, and the content type of the last upload (webp).
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");

    // An outsider (not enrolled) can't read the student's own route.
    let (status, _, _) =
        common::send_raw(&app, "GET", &own, Some(&outsider), None, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "outsiders can't peek");

    // The drawing rides the student's own answer state — save the text answer
    // it accompanies, then the sitting view embeds the drawing meta.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "a cell, drawn and described" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body[0]["answer"]["answer_image"]["content_type"],
        "image/webp"
    );

    // The teacher grading sheet carries the drawing meta and serves the bytes
    // via the teacher route; the sheet row also exposes the flag.
    let teacher_bytes = format!("/exams/{exam}/attempts/{student_id}/answers/{question}/image");
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["answers"][0]["answer_image"]["content_type"],
        "image/webp"
    );
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &teacher_bytes,
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png);
    // An outsider student can't read the teacher grading route (not a teacher).
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &teacher_bytes,
        Some(&outsider),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A retake keeps the prior sitting's sheet — per-attempt history — so the
    // seq-1 answer-image row *and* its on-disk blob survive the new sitting.
    let pre = answer_image_blob_keys(&db).await;
    assert_eq!(pre.len(), 1);
    assert!(common::files_dir().join(&pre[0]).exists());
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "retake: {}", res.body);
    assert_eq!(res.body["attempt"], 2, "the retake is the second sitting");
    assert_eq!(
        answer_image_blob_keys(&db).await.len(),
        1,
        "the retake kept the prior sitting's answer-image row (history)"
    );
    assert!(
        common::files_dir().join(&pre[0]).exists(),
        "the retake kept the prior sitting's answer-image blob on disk"
    );

    // A fresh drawing in the new sitting adds a second row alongside the kept
    // one; deleting the exam then sweeps every sitting's rows and blobs.
    let (status, _) = post_image(&app, &student, &own, "image/png", png).await;
    assert_eq!(status, StatusCode::CREATED);
    let keys = answer_image_blob_keys(&db).await;
    assert_eq!(keys.len(), 2, "seq-1 and seq-2 drawings coexist");
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
        answer_image_blob_keys(&db).await.is_empty(),
        "answer-image rows survived the exam delete"
    );
    for key in &keys {
        assert!(
            !common::files_dir().join(key).exists(),
            "answer-image blob {key} survived the exam delete"
        );
    }
}

/// A student who draws but types nothing still has their drawing surface in
/// both the sitting view and the teacher grading sheet — the upload creates the
/// backing (blank-text) answer row, since `answer_image` rides the answer payload.
#[tokio::test]
async fn answer_image_surfaces_for_a_drawing_only_answer() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "draw_t", "teacher").await;
    let student = login(&app, "draw_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "art").await;
    let subject = create_subject(&app, &teacher, &course, "drawing").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "sketch", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Draw a triangle.", "kind": "text", "points": 5 }),
    )
    .await;
    let question = id_of(&question_body);

    // Sit, then upload a drawing WITHOUT ever saving a text answer.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let own = format!("/exams/{exam}/attempt/answers/{question}/image");
    let (status, body) = post_image(&app, &student, &own, "image/png", b"tri").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The sitting view carries the drawing meta with an empty typed answer.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body[0]["answer"]["answer_image"]["content_type"],
        "image/png"
    );
    assert_eq!(res.body[0]["answer"]["text"], "");

    // The teacher grading sheet shows it too.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["answers"][0]["answer_image"]["content_type"],
        "image/png"
    );

    // Removing the drawing drops the blank answer row it created — the question
    // stops showing as answered rather than lingering as an empty text answer.
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body[0]["answer"].is_null(),
        "blank answer row dropped with its drawing"
    );
}

/// The authoring edits that reshape a question also groom its images — and
/// **only** the pictures of options that are actually gone. Reordering the list
/// and deleting one option leaves every surviving option's picture attached to
/// *that option*, and leaves `correct` naming the option the author marked.
/// This is the behaviour stable choice ids exist for: the images used to be
/// keyed by position, so any edit that carried a `choices` key wiped all of
/// them.
#[tokio::test]
async fn question_image_edits_follow_the_choices() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "imge_t", "teacher").await;
    let course = create_course(&app, &teacher, "history").await;
    let subject = create_subject(&app, &teacher, &course, "eras").await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "eras", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Pick the era.", "kind": "choice", "points": 5,
                "choices": [{"id": "c0", "text": "Stone"}, {"id": "c1", "text": "Bronze"},
                            {"id": "c2", "text": "Iron"}], "correct": "c2" }),
    )
    .await;
    let question = id_of(&question_body);
    let (stone, bronze, iron) = (
        choice_of(&question_body, 0),
        choice_of(&question_body, 1),
        choice_of(&question_body, 2),
    );
    let base = format!("/exams/{exam}/questions/{question}");
    for (path, bytes) in [
        (format!("{base}/image"), b"illustration".as_slice()),
        (format!("{base}/choices/{stone}/image"), b"stone".as_slice()),
        (
            format!("{base}/choices/{bronze}/image"),
            b"bronze".as_slice(),
        ),
        (format!("{base}/choices/{iron}/image"), b"iron".as_slice()),
    ] {
        let (status, body) = post_image(&app, &teacher, &path, "image/png", bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // A points-only PATCH touches no images.
    let res = send(
        &app,
        "PATCH",
        &base,
        Some(&teacher),
        Some(json!({ "points": 7 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["image"].is_object());
    for slot in 0..3 {
        assert!(res.body["choice_images"][slot].is_object());
    }
    assert_eq!(res.body["correct"], iron, "the answer key must not move");

    // The edit this whole remodel exists for: reorder the list (Iron first),
    // rename one option, and drop Bronze entirely.
    let keys_before = image_blob_keys(&db).await;
    let res = send(
        &app,
        "PATCH",
        &base,
        Some(&teacher),
        Some(json!({ "choices": [
            {"id": iron, "text": "Iron Age"},
            {"id": stone, "text": "Stone"},
        ], "correct": iron })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // The surviving options kept their identities, in their new order…
    assert_eq!(res.body["choices"][0]["id"], iron);
    assert_eq!(res.body["choices"][0]["text"], "Iron Age");
    assert_eq!(res.body["choices"][1]["id"], stone);
    // …the answer key still names the option the author marked, though it moved
    // from position 2 to position 0…
    assert_eq!(res.body["correct"], iron, "correct must follow its option");
    // …and each survivor kept *its own* picture, illustration included.
    assert!(res.body["image"].is_object(), "illustration must survive");
    assert!(
        res.body["choice_images"][0].is_object(),
        "Iron kept its picture across the reorder"
    );
    assert!(
        res.body["choice_images"][1].is_object(),
        "Stone kept its picture across the reorder"
    );

    // Exactly one blob went: the removed option's.
    let keys_after = image_blob_keys(&db).await;
    assert_eq!(
        keys_after.len(),
        3,
        "illustration + two surviving option pictures"
    );
    let dropped: Vec<_> = keys_before
        .iter()
        .filter(|k| !keys_after.contains(k))
        .collect();
    assert_eq!(dropped.len(), 1, "only the removed option's blob may go");
    for key in dropped {
        assert!(
            !common::files_dir().join(key).exists(),
            "dropped option blob lingers"
        );
    }

    // The remaining wipe path, now opt-in: a list of all-new options (no id
    // matches a stored one) is a genuinely new set, so every option picture
    // goes — and the illustration still stays.
    let res = send(
        &app,
        "PATCH",
        &base,
        Some(&teacher),
        Some(json!({ "choices": [{"id": "fresh-a", "text": "Iron"}, {"id": "fresh-b", "text": "Stone"}],
                     "correct": "fresh-a" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_ne!(res.body["choices"][0]["id"], iron, "a new option, not Iron");
    assert_eq!(
        res.body["choice_images"],
        json!([null, null]),
        "an all-new option list keeps no pictures"
    );
    assert!(res.body["image"].is_object(), "illustration must survive");
    assert_eq!(image_blob_keys(&db).await.len(), 1);

    // Explicit image DELETE: 204 once, 404 after, blob gone.
    let fresh = choice_of(&res.body, 0);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("{base}/choices/{fresh}/image"),
        "image/gif",
        b"gif",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let res = send(
        &app,
        "DELETE",
        &format!("{base}/choices/{fresh}/image"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{base}/choices/{fresh}/image"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Deleting the question sweeps its remaining image rows and blobs.
    let keys = image_blob_keys(&db).await;
    assert_eq!(keys.len(), 1);
    let res = send(&app, "DELETE", &base, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(image_blob_keys(&db).await.is_empty());
    for key in &keys {
        assert!(
            !common::files_dir().join(key).exists(),
            "blob {key} survived the question delete"
        );
    }
}

// --- exam drafts ---------------------------------------------------------

#[tokio::test]
async fn draft_exams_hide_from_students_until_published() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "draft_t", "teacher").await;
    let student = login(&app, "draft_s").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "history").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // Saved as a draft: an open (sittable-were-it-published) exam.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "wip final", "kind": "final", "mode": "open", "draft": true }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["draft"], true);
    let exam = id_of(&res.body);

    // The manager sees it everywhere, flagged.
    let mine = send(&app, "GET", "/exams", Some(&teacher), None).await;
    assert_eq!(common::items(&mine.body).len(), 1);
    assert_eq!(common::items(&mine.body)[0]["draft"], true);

    // The enrolled student sees nothing: not in either list, a 404 on direct
    // read, and a 404 (not a telltale 409) on sitting.
    let listed = send(&app, "GET", "/exams", Some(&student), None).await;
    assert_eq!(common::items(&listed.body).len(), 0);
    let in_course = send(
        &app,
        "GET",
        &format!("/courses/{course}/exams"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(common::items(&in_course.body).len(), 0);
    for (method, path) in [
        ("GET", format!("/exams/{exam}")),
        ("POST", format!("/exams/{exam}/attempt")),
    ] {
        let res = send(&app, method, &path, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    // Grading a draft is refused — a mark must never point at a hidden exam.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Publish. The student now sees and sits it.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "draft": false })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["draft"], false);
    let listed = send(&app, "GET", "/exams", Some(&student), None).await;
    assert_eq!(common::items(&listed.body).len(), 1);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // Once sat, the exam can't be pulled back into hiding.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "draft": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn results_alone_also_freeze_re_drafting() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "redraft_t", "teacher").await;
    let student = login(&app, "redraft_s").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "geo").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // A published offline-graded exam (no mode, no attempts — marks by hand).
    let exam = create_exam(&app, &teacher, &course, "field trip report", "project").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 75, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // No attempts exist, but the graded result freezes re-drafting too.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "draft": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Ungraded siblings still re-draft freely.
    let other = create_exam(&app, &teacher, &course, "map quiz", "quiz").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{other}"),
        Some(&teacher),
        Some(json!({ "draft": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["draft"], true);
}

// --- parent role ---------------------------------------------------------

/// The tie lifecycle: admin-only, role-checked on both ends, idempotent, and
/// symmetric between the admin listing and the parent's own `/me/students`.
#[tokio::test]
async fn parent_links_are_admin_managed_and_role_checked() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let teacher_id = me_id(&app, &teacher).await;

    // Only an admin ties.
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&teacher),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Both ends are role-checked: the target must be a parent…
    let res = send(
        &app,
        "POST",
        &format!("/users/{teacher_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // …the student must be a student…
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // …and both must exist.
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": "01ZZZZZZZZZZZZZZZZZZZZZZZZ" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/users/01ZZZZZZZZZZZZZZZZZZZZZZZZ/students",
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A valid tie lands, and linking again is a no-op returning the same tie.
    for _ in 0..2 {
        let res = send(
            &app,
            "POST",
            &format!("/users/{parent_id}/students"),
            Some(&admin),
            Some(json!({ "user_id": ali_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["student"]["id"], json!(ali_id));
        assert_eq!(res.body["parent"]["id"], json!(parent_id));
    }

    // Admin listing and the parent's own listing agree.
    let res = send(
        &app,
        "GET",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1);
    let res = send(&app, "GET", "/users/me/students", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body)[0]["id"], json!(ali_id));
    // /me/students is the parent's view — everyone else is refused.
    let res = send(&app, "GET", "/users/me/students", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Untie: once, then the tie is gone.
    let res = send(
        &app,
        "DELETE",
        &format!("/users/{parent_id}/students/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/users/{parent_id}/students/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// The tie is the parent's whole power: linked students' reports open up (in
/// full — no course narrowing), everyone else's stay walled, and every write
/// a parent might try is refused.
#[tokio::test]
async fn parent_observes_linked_students_and_nothing_else() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let bob = login(&app, "bob").await;
    let bob_id = me_id(&app, &bob).await;

    // ali gets a graded exam in a course the parent has nothing to do with.
    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &ali_id).await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "quiz").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Linked student: all three reports open, and the mark report is FULL —
    // the parent manages no courses, so narrowing would blank it.
    let res = send(
        &app,
        "GET",
        &format!("/marks/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["courses"][0]["results"][0]["mark"], 70);
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/pomodoro/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Unlinked student: every report stays walled.
    for uri in [
        format!("/marks/{bob_id}"),
        format!("/attendance/{bob_id}"),
        format!("/pomodoro/{bob_id}"),
    ] {
        let res = send(&app, "GET", &uri, Some(&parent), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri} must stay walled");
    }

    // A parent writes nothing: not a student (pomodoro, sitting), not staff
    // (courses, grading, search), and the tie grants no write-through.
    let res = send(&app, "POST", "/pomodoro/start", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&parent),
        Some(json!({ "title": "sneaky" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // A sittable exam (the modeless `exam` would 409 as unsittable before any
    // role question arises): the student-only wall answers first.
    let sittable = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "anytime", "kind": "quiz", "mode": "open" }),
    )
    .await;
    assert_eq!(sittable.status, StatusCode::CREATED, "{}", sittable.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{}/attempt", id_of(&sittable.body)),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(&app, "GET", "/users/search?q=ali", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

/// A role change off either end of a tie sweeps it, exactly like promotion
/// sweeps enrollments — no dead read grants left behind.
#[tokio::test]
async fn role_change_sweeps_parent_links() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The student side leaves the student role: the tie dies with it.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/users/me/students", Some(&parent), None).await;
    assert_eq!(common::total(&res.body), 0, "tie must die with the role");
    let res = send(
        &app,
        "GET",
        &format!("/marks/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The parent side leaves the parent role: its remaining ties die too.
    let bob = login(&app, "bob").await;
    let bob_id = me_id(&app, &bob).await;
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": bob_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{parent_id}/role"),
        Some(&admin),
        Some(json!({ "role": "student" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Really deleted, not merely hidden by the role checks.
    let mut result = db
        .query("SELECT VALUE id FROM parent_link")
        .await
        .expect("count parent links")
        .check()
        .expect("count parent links check");
    let rows: Vec<surrealdb::types::RecordId> = result.take(0).expect("parent link rows");
    assert!(rows.is_empty(), "parent_link rows deleted from the DB");
}

// --- messages ------------------------------------------------------------

#[tokio::test]
async fn messages_flow_through_folders_per_side() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // student
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    // Send lands in the recipient's inbox unread; the sender holds a `sent` copy.
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": hoca_id, "subject": "etüt", "body": "10 dk gecikebilirim", "label": " Etüt " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["folder"], "sent");
    assert_eq!(res.body["read"], false);
    assert_eq!(res.body["label"], "Etüt", "label stored trimmed");
    assert_eq!(res.body["sender"]["username"], "ali");
    assert_eq!(res.body["sender_role"], "student");
    assert_eq!(res.body["recipient_role"], "teacher");
    let msg_id = id_of(&res.body);

    // Recipient's inbox shows it; their copy says `inbox`.
    let inbox = send(&app, "GET", "/messages", Some(&hoca), None).await;
    let items = common::items(&inbox.body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["folder"], "inbox");
    assert_eq!(items[0]["subject"], "etüt");

    // Only the recipient flips the read flag; the sender sees the receipt.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // `?read=` narrows; `total` on the filtered view is the unread badge.
    let unread = send(
        &app,
        "GET",
        "/messages?folder=inbox&read=false&limit=1",
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(unread.body["total"], 1);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let unread = send(
        &app,
        "GET",
        "/messages?folder=inbox&read=false&limit=1",
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(unread.body["total"], 0);
    let sent = send(&app, "GET", "/messages?folder=sent", Some(&ali), None).await;
    assert_eq!(common::items(&sent.body)[0]["read"], true);

    // Each side only moves through its own folders.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "sent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "archive" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let inbox = send(&app, "GET", "/messages", Some(&hoca), None).await;
    assert_eq!(common::items(&inbox.body).len(), 0);
    let archive = send(&app, "GET", "/messages?folder=archive", Some(&hoca), None).await;
    assert_eq!(common::items(&archive.body).len(), 1);

    // Permanent delete only from the trash; it never touches the other copy.
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "trash" })),
    )
    .await;
    let trash = send(&app, "GET", "/messages?folder=trash", Some(&hoca), None).await;
    assert_eq!(common::items(&trash.body).len(), 1);
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let trash = send(&app, "GET", "/messages?folder=trash", Some(&hoca), None).await;
    assert_eq!(common::items(&trash.body).len(), 0);
    // Deleted side is no longer a party at all.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "inbox" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    // Sender still holds their copy…
    let sent = send(&app, "GET", "/messages?folder=sent", Some(&ali), None).await;
    assert_eq!(common::items(&sent.body).len(), 1);

    // …and once the sender trashes + deletes too, the row itself is gone.
    send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        Some(json!({ "folder": "trash" })),
    )
    .await;
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let mut result = db
        .query("SELECT VALUE id FROM message")
        .await
        .expect("count messages")
        .check()
        .expect("count messages check");
    let rows: Vec<surrealdb::types::RecordId> = result.take(0).expect("message rows");
    assert!(rows.is_empty(), "both-sides-deleted row is removed");
}

/// A filed copy remembers the folder it left, so restoring lands where it
/// came from instead of always in the inbox.
#[tokio::test]
async fn messages_remember_the_folder_a_filed_copy_came_from() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // student
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": hoca_id, "subject": "etüt" })),
    )
    .await;
    let msg_id = res.body["id"].as_str().expect("message id").to_string();
    assert!(
        res.body["previous_folder"].is_null(),
        "a home copy has none"
    );

    let file = |actor: &'static str, folder: &'static str| {
        let app = app.clone();
        let (ali, hoca) = (ali.clone(), hoca.clone());
        let msg_id = msg_id.clone();
        async move {
            let session = if actor == "ali" { ali } else { hoca };
            let res = send(
                &app,
                "PATCH",
                &format!("/messages/{msg_id}"),
                Some(&session),
                Some(json!({ "folder": folder })),
            )
            .await;
            assert_eq!(res.status, StatusCode::OK, "move to {folder}");
            res.body["previous_folder"].clone()
        }
    };

    // inbox → archive → trash walks the memory one hop at a time.
    assert_eq!(file("hoca", "archive").await, json!("inbox"));
    assert_eq!(file("hoca", "trash").await, json!("archive"));
    // Restoring to the remembered folder puts the copy back there — with no
    // memory of its own, since it came out of the trash and the trash is
    // never a restore target. It falls back to the home folder.
    assert!(file("hoca", "archive").await.is_null());
    let archive = send(&app, "GET", "/messages?folder=archive", Some(&hoca), None).await;
    assert_eq!(common::items(&archive.body).len(), 1);
    // Back home clears the stamp for good.
    assert!(file("hoca", "inbox").await.is_null());

    // A no-op move keeps the memory it already had.
    assert_eq!(file("hoca", "archive").await, json!("inbox"));
    assert_eq!(file("hoca", "archive").await, json!("inbox"));

    // The sender's side has only one home, so its trash always points at it.
    assert_eq!(file("ali", "trash").await, json!("sent"));
    // The two sides' memories are independent.
    let trash = send(&app, "GET", "/messages?folder=trash", Some(&ali), None).await;
    assert_eq!(common::items(&trash.body)[0]["previous_folder"], "sent");
    let archive = send(&app, "GET", "/messages?folder=archive", Some(&hoca), None).await;
    assert_eq!(common::items(&archive.body)[0]["previous_folder"], "inbox");

    // Deleting drops the stamp with the rest of that side's copy.
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let mut result = db
        .query("SELECT VALUE sender_origin FROM message")
        .await
        .expect("read origins")
        .check()
        .expect("read origins check");
    let origins: Vec<Option<String>> = result.take(0).expect("origin rows");
    assert_eq!(origins, vec![None], "the deleted side keeps no origin");
}

#[tokio::test]
async fn messages_guard_parties_recipients_and_folders() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let parent = login_as(&app, &db, "anne", "parent").await;

    // Sending to yourself or to nobody fails.
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": ali_id, "subject": "hi" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": "01J8XZ0K3Q8G7X2M4N5P6R7S8T", "subject": "hi" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A parent writes like anyone else — messaging is the role's one pen.
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&parent),
        Some(json!({ "recipient_id": ali_id, "subject": "görüşme", "body": "uygun mu?" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["sender_role"], "parent");
    assert_eq!(
        res.body["label"],
        serde_json::Value::Null,
        "no label sent, none stored"
    );
    let msg_id = id_of(&res.body);

    // A third user is not a party: sees nothing, touches nothing.
    let inbox = send(&app, "GET", "/messages", Some(&veli), None).await;
    assert_eq!(common::items(&inbox.body).len(), 0);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&veli),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Unknown folder names and oversized labels are refused.
    let res = send(&app, "GET", "/messages?folder=spam", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let veli_id = me_id(&app, &veli).await;
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": veli_id, "subject": "hi", "label": "x".repeat(51) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

/// A patch that fails validation must not half-apply: the read flag stays
/// untouched when the folder part of the same body is rejected.
#[tokio::test]
async fn messages_rejected_patch_leaves_no_side_effects() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": hoca_id, "subject": "s", "body": "b" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let msg_id = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Recipient can't move to `sent`; the bundled read flip must not land.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "read": false, "folder": "sent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let unread = send(
        &app,
        "GET",
        "/messages?folder=inbox&read=false",
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(
        common::items(&unread.body).len(),
        0,
        "rejected patch must not flip the read flag"
    );
}

/// The read-only parent role must bounce off every notes surface — they were
/// gated on bare authentication before the parent role existed.
#[tokio::test]
async fn parent_role_cannot_touch_notes() {
    let (app, db) = app_and_db().await;
    let parent = login_as(&app, &db, "baba", "parent").await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&parent),
        Some(json!({ "title": "not" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(&app, "GET", "/notes", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Student and up keep the pen.
    let ali = login(&app, "ali").await;
    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "ok" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// A parent link whose student side stopped being a student (a role sweep
/// losing a race with link creation) must be inert, exactly like a stale
/// enrollment: the report reads re-check the live role.
#[tokio::test]
async fn stale_parent_link_is_inert_after_role_change() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/marks/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Promote the student straight in the DB — the endpoint's sweep never
    // fires, leaving the link row behind like a lost race would.
    common::set_role(&db, "ali", "teacher").await;

    for path in [
        format!("/marks/{ali_id}"),
        format!("/attendance/{ali_id}"),
        format!("/pomodoro/{ali_id}"),
    ] {
        let res = send(&app, "GET", &path, Some(&parent), None).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "stale link must not grant {path}"
        );
    }
}

// --- the question pool ---------------------------------------------------

/// The pool lifecycle end to end: a student asks (pending — invisible to
/// other students, listed for teachers), a teacher approves (one-way, 409 on
/// repeat), the pool opens school-wide, anyone offers solutions, and
/// moderation deletes cascade.
#[tokio::test]
async fn question_pool_lifecycle() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let stranger = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    // Only students ask: the teacher is refused.
    let ask = json!({ "title": "Integral", "body": "∫x·eˣ dx?" });
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&teacher),
        Some(ask.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    let res = send(&app, "POST", "/questions", Some(&asker), Some(ask)).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    let qid = id_of(&res.body);

    // Pending: the asker and the teacher see it; another student must not
    // even learn it exists — and it takes no solutions yet.
    for (cookie, visible) in [(&asker, true), (&teacher, true), (&stranger, false)] {
        let res = send(
            &app,
            "GET",
            &format!("/questions/{qid}"),
            Some(cookie),
            None,
        )
        .await;
        let expected = if visible {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(res.status, expected, "{}", res.body);
    }
    let offer = json!({ "body": "Kısmi integrasyon." });
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&asker),
        Some(offer.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // The teacher's ?status=pending list is the approval queue; approval is
    // teacher-gated and one-way.
    let res = send(
        &app,
        "GET",
        "/questions?status=pending",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&stranger),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["approved_by"]["username"], "hoca");
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Approved: school-wide — the stranger reads it and answers it.
    let res = send(&app, "GET", "/questions", Some(&stranger), None).await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&stranger),
        Some(offer),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);

    // Deleting the solution: a third party may not, its author may.
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&stranger),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // Question delete (asker withdraws) cascades what's left.
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// Parents are observers: every pool surface (list, ask, answer) is closed to
/// them.
#[tokio::test]
async fn question_pool_locks_parents_out() {
    let (app, db) = app_and_db().await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let res = send(&app, "GET", "/questions", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&parent),
        Some(json!({ "title": "t", "body": "b" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// The question photo: asker-only upload while pending, SVG refused, bytes
/// served under the question's visibility, approval freezes the slot.
#[tokio::test]
async fn question_pool_image_upload_and_freeze() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let stranger = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Harita", "body": "Hangi şehir?" })),
    )
    .await;
    let qid = id_of(&res.body);
    let upload = |cookie: &str, content_type: &str| {
        let uri = format!("/questions/{qid}/image");
        let body = common::multipart_file("soru.png", content_type, b"png-bytes");
        let cookie = cookie.to_string();
        let app = app.clone();
        async move {
            common::send_raw(
                &app,
                "POST",
                &uri,
                Some(&cookie),
                Some("multipart/form-data; boundary=hezarfen-test-boundary"),
                body,
            )
            .await
        }
    };

    // SVG is out; the teacher isn't the asker; the asker's PNG lands.
    let (status, _, _) = upload(&asker, "image/svg+xml").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = upload(&teacher, "image/png").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&asker, "image/png").await;
    assert_eq!(status, StatusCode::CREATED);

    // Pending bytes follow pending visibility: asker yes, stranger no.
    let uri = format!("/questions/{qid}/image");
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(bytes, b"png-bytes");
    let (status, _, _) =
        common::send_raw(&app, "GET", &uri, Some(&stranger), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Approval freezes the image with the text: replace and remove both 409.
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let (status, _, _) = upload(&asker, "image/png").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // But the approved bytes are now school-wide, and ride the question meta.
    let (status, _, _) =
        common::send_raw(&app, "GET", &uri, Some(&stranger), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}"),
        Some(&stranger),
        None,
    )
    .await;
    assert_eq!(res.body["image"]["content_type"], "image/png");
}

/// The solution's photo and body stay the author's alone (no freeze — no
/// moderation state to protect), bytes follow the question's visibility, and
/// question rows tally their solutions.
#[tokio::test]
async fn solution_images_edits_and_counts() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    // An approved question with the helper's solution under it.
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Limit", "body": "x→0 iken sin(x)/x?" })),
    )
    .await;
    let qid = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "Birebir 1'e gider." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert!(res.body["image"].is_null());
    let sid = id_of(&res.body);

    // Image writes are the author's: the asker is refused, the helper lands.
    let uri = format!("/questions/{qid}/solutions/{sid}/image");
    let upload = |cookie: &str| {
        let uri = uri.clone();
        let body = common::multipart_file("cozum.png", "image/png", b"png-bytes");
        let cookie = cookie.to_string();
        let app = app.clone();
        async move {
            common::send_raw(
                &app,
                "POST",
                &uri,
                Some(&cookie),
                Some("multipart/form-data; boundary=hezarfen-test-boundary"),
                body,
            )
            .await
        }
    };
    let (status, _, _) = upload(&asker).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&teacher).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&helper).await;
    assert_eq!(status, StatusCode::CREATED);

    // Bytes follow the question's visibility — approved, so any student
    // reads them — and the meta rides the solution row.
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(bytes, b"png-bytes");
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}/solutions"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.body["items"][0]["image"]["content_type"], "image/png");

    // Body edits are author-only too — teacher+ moderate by deleting, never
    // by rewriting.
    let edit = json!({ "body": "Standart limit: sin(x)/x → 1." });
    let res = send(
        &app,
        "PATCH",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&asker),
        Some(edit.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&helper),
        Some(edit),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["body"], "Standart limit: sin(x)/x → 1.");
    assert_eq!(res.body["image"]["content_type"], "image/png");

    // Question rows tally their solutions, in the list and the single GET.
    let res = send(&app, "GET", "/questions?limit=5", Some(&asker), None).await;
    assert_eq!(res.body["items"][0]["solution_count"], 1);
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.body["solution_count"], 1);

    // The author drops the photo; a second removal finds nothing.
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &uri, Some(&helper), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &uri, Some(&helper), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And the question delete still sweeps everything under it.
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

// --- drawing byte-fidelity (stroke-JSON tEXt passthrough) -------------------
// The frontend's canvas saves a drawing as a PNG that also carries its
// editable stroke JSON inside a custom `tEXt` chunk; the backend only ever
// stores and re-serves the uploaded bytes (see `read_upload` / `serve_inline_blob`
// in web/mod.rs). These tests build a real (if minimal) PNG with a `tEXt`
// chunk and assert the round trip is byte-for-byte — if anyone ever adds
// image re-encoding, compression, or thumbnailing, every stored drawing
// silently stops being replayable, and this is what would catch it.

/// CRC-32 (ISO-HDLC — the checksum PNG chunks and zlib both use), bit by bit:
/// no table, since this only ever runs over a few dozen fixture bytes. No new
/// crate for one checksum used to hand-build a PNG below.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Pin the hand-rolled checksum against the standard CRC-32 check value.
/// Nothing downstream ever validates a chunk's CRC (the server never parses
/// PNG structure), so a broken implementation here would silently build
/// fixtures that only *look* like PNGs — this is the one thing that would
/// catch that.
#[tokio::test]
async fn crc32_matches_the_standard_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

/// Frame one PNG chunk: 4-byte big-endian length, 4-byte type, the data, then
/// the CRC-32 over type+data.
fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let crc_body: Vec<u8> = kind.iter().chain(data.iter()).copied().collect();
    chunk.extend_from_slice(&crc32(&crc_body).to_be_bytes());
    chunk
}

/// A genuinely valid, minimal (1x1 grayscale) PNG carrying a custom `tEXt`
/// chunk whose text is `marker` — the same trick the frontend uses to smuggle
/// its editable stroke JSON inside a drawing's PNG bytes. Built inline (no
/// binary fixture file) so the test commits exactly the bytes it later
/// asserts on.
fn png_with_text_chunk(marker: &str) -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    // IHDR: 1x1 pixel, 8-bit depth, grayscale, no interlace.
    png.extend(png_chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]));
    // tEXt: keyword "Comment", NUL, then the marker — exactly where a real
    // drawing's stroke JSON rides.
    let mut text = b"Comment\0".to_vec();
    text.extend_from_slice(marker.as_bytes());
    png.extend(png_chunk(b"tEXt", &text));
    // IDAT: one stored (uncompressed) deflate block over the single
    // [filter=None, pixel=0] scanline, so this decodes as a real black pixel.
    png.extend(png_chunk(
        b"IDAT",
        &[
            0x78, 0x01, // zlib header
            0x01, 0x02, 0x00, 0xFD, 0xFF, // stored block: BFINAL+BTYPE, LEN, NLEN
            0x00, 0x00, // scanline: filter=None, pixel=0
            0x00, 0x02, 0x00, 0x01, // Adler-32 of the scanline bytes
        ],
    ));
    png.extend(png_chunk(b"IEND", &[]));
    png
}

/// The primary invariant: a student's answer drawing survives its round trip
/// byte-for-byte, `tEXt` chunk included — on both read paths that matter, the
/// student's own (for editing) and the teacher's grading read (for replay).
/// If the backend ever re-encodes or strips metadata from this PNG, the
/// stroke JSON in the `tEXt` chunk is gone and the drawing stops replaying.
#[tokio::test]
async fn answer_image_round_trip_keeps_the_stroke_text_chunk_intact() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rt_t", "teacher").await;
    let student = login(&app, "rt_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "art").await;
    let subject = create_subject(&app, &teacher, &course, "sketching").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "sketch quiz", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Draw a triangle.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let png = png_with_text_chunk(r#"{"strokes":[[1,1],[2,2],[3,3]]}"#);
    let own = format!("/exams/{exam}/attempt/answers/{question}/image");
    let (status, body) = post_image(&app, &student, &own, "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);

    // The student replays their own drawing from this route.
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(bytes, png, "the student's own read must be byte-identical");

    // The teacher replays it too, via the grading route — same requirement.
    let teacher_uri = format!("/exams/{exam}/attempts/{student_id}/answers/{question}/image");
    let (status, _, bytes) =
        common::send_raw(&app, "GET", &teacher_uri, Some(&teacher), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png, "the grader read must be byte-identical too");
}

/// The pool question photo is a drawing too (a student can attach a photo or
/// a canvas sketch of the problem) — same byte-for-byte requirement as an
/// answer image, or a teacher replaying a hand-drawn question loses the
/// stroke JSON in its `tEXt` chunk.
#[tokio::test]
async fn question_photo_round_trip_keeps_the_stroke_text_chunk_intact() {
    let app = mem_app().await;
    let asker = login(&app, "ali").await;
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Şekil", "body": "Çizim ekli" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid = id_of(&res.body);

    let png = png_with_text_chunk(r#"{"strokes":[[4,4],[5,5]]}"#);
    let uri = format!("/questions/{qid}/image");
    let (status, body) = post_image(&app, &asker, &uri, "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);

    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(
        bytes, png,
        "the pool question photo must round-trip byte-for-byte"
    );
}

/// Same story on a solution's worked-steps photo: the backend must never
/// touch these bytes, or a replayed solution drawing loses its stroke JSON.
#[tokio::test]
async fn solution_photo_round_trip_keeps_the_stroke_text_chunk_intact() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Limit", "body": "Çözüm arıyorum" })),
    )
    .await;
    let qid = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "İşte çözüm." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);

    let png = png_with_text_chunk(r#"{"strokes":[[6,6],[7,7]]}"#);
    let uri = format!("/questions/{qid}/solutions/{sid}/image");
    let (status, body) = post_image(&app, &helper, &uri, "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);

    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(
        bytes, png,
        "the solution photo must round-trip byte-for-byte"
    );
}

/// The contrast to `question_pool_image_upload_and_freeze`'s 409-after-approval:
/// a solution carries no moderation state, so its author replaces the photo
/// freely at any time, even on a long-approved question.
#[tokio::test]
async fn solution_photo_replaces_freely_unlike_the_frozen_question_photo() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Türev", "body": "Nasıl alınır?" })),
    )
    .await;
    let qid = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "Cevap burada." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);

    let uri = format!("/questions/{qid}/solutions/{sid}/image");
    let (status, _) = post_image(&app, &helper, &uri, "image/png", b"first-draft").await;
    assert_eq!(status, StatusCode::CREATED);
    // No freeze: a second replace on the same long-approved question still lands.
    let (status, _) = post_image(&app, &helper, &uri, "image/png", b"second-draft").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a solution photo must never freeze"
    );
    let (status, _, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&helper), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"second-draft");
}

/// The other half of a pending question's photo visibility (the asker-vs-stranger
/// half is already pinned by `question_pool_image_upload_and_freeze`): teacher+
/// is the approval queue's other reader, so a pending photo must reach them too.
#[tokio::test]
async fn pool_question_pending_photo_reaches_teacher_too() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Soru", "body": "Yardım lazım" })),
    )
    .await;
    let qid = id_of(&res.body);
    let uri = format!("/questions/{qid}/image");
    let (status, _) = post_image(&app, &asker, &uri, "image/png", b"png-bytes").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&teacher), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "teacher+ must see a pending photo too"
    );
    assert_eq!(bytes, b"png-bytes");
}

/// `GET` on a pool photo needs student role or higher; parents are read-only
/// report observers (see `question_pool_locks_parents_out`), so both photo
/// routes must refuse them exactly like the rest of the pool.
#[tokio::test]
async fn pool_photo_reads_require_student_role() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;
    let parent = login_as(&app, &db, "anne", "parent").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Soru", "body": "Yardım lazım" })),
    )
    .await;
    let qid = id_of(&res.body);
    let q_uri = format!("/questions/{qid}/image");
    let (status, _) = post_image(&app, &asker, &q_uri, "image/png", b"q-bytes").await;
    assert_eq!(status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "Cevap burada." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);
    let s_uri = format!("/questions/{qid}/solutions/{sid}/image");
    let (status, _) = post_image(&app, &helper, &s_uri, "image/png", b"s-bytes").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) =
        common::send_raw(&app, "GET", &q_uri, Some(&parent), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "parents can't read a question photo"
    );
    let (status, _, _) =
        common::send_raw(&app, "GET", &s_uri, Some(&parent), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "parents can't read a solution photo"
    );
}

// --- homework -------------------------------------------------------------

/// A teacher-run course with a subject and two enrolled students — the stage
/// most homework tests play on.
struct HwWorld {
    teacher: String,
    ali: String,
    ali_id: String,
    veli: String,
    veli_id: String,
    course: String,
    subject: String,
}

async fn hw_world(app: &axum::Router, db: &hezarfen_backend::database::Database) -> HwWorld {
    let teacher = login_as(app, db, "teacher", "teacher").await;
    let ali = login(app, "ali").await;
    let ali_id = me_id(app, &ali).await;
    let veli = login(app, "veli").await;
    let veli_id = me_id(app, &veli).await;
    let course = create_course(app, &teacher, "math").await;
    let subject = create_subject(app, &teacher, &course, "algebra").await;
    enroll(app, &teacher, &course, &ali_id).await;
    enroll(app, &teacher, &course, &veli_id).await;
    HwWorld {
        teacher,
        ali,
        ali_id,
        veli,
        veli_id,
        course,
        subject,
    }
}

/// POST the caller's submission (optional text) to `hw` — no assertion.
async fn submit_hw(app: &axum::Router, cookie: &str, hw: &str, text: Option<&str>) -> common::Res {
    let body = match text {
        Some(text) => json!({ "text": text }),
        None => json!({}),
    };
    send(
        app,
        "POST",
        &format!("/homework/{hw}/submission"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// Upload `bytes` as a file onto the caller's submission to `hw` — no
/// assertion. Parses the JSON body like `send`.
async fn upload_hw_file(
    app: &axum::Router,
    cookie: &str,
    hw: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> common::Res {
    let (status, _, body) = common::send_raw(
        app,
        "POST",
        &format!("/homework/{hw}/submission/files"),
        Some(cookie),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        common::multipart_file(filename, content_type, bytes),
    )
    .await;
    let body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    common::Res {
        status,
        body,
        cookie: None,
    }
}

/// POST a grade for `user` onto `hw` — no assertion.
async fn grade_hw(
    app: &axum::Router,
    cookie: &str,
    hw: &str,
    user: &str,
    status: &str,
    mark: Option<i64>,
) -> common::Res {
    let mut body = json!({ "user": user, "status": status });
    if let Some(mark) = mark {
        body["mark"] = json!(mark);
    }
    send(
        app,
        "POST",
        &format!("/homework/{hw}/results"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// Every stored homework-file blob name, straight from the table.
async fn homework_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM homework_file")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Row count of `table`, straight from the database — a cascade must really
/// delete, not merely hide behind the role checks.
async fn hw_row_count(db: &hezarfen_backend::database::Database, table: &str) -> usize {
    let mut result = db
        .query(format!("SELECT VALUE id FROM {table}"))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len()
}

/// Creation validates the due date (60s grace), the subject's course, the
/// subset's enrollment, and the subset cap — and stays teacher-only.
#[tokio::test]
async fn homework_create_validates_due_subject_and_subset() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    // due_at beyond the 60s grace lies in the past — rejected.
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "old", "subject_id": w.subject, "due_at": now - 120_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // The subject must belong to this very course.
    let other_course = create_course(&app, &w.teacher, "physics").await;
    let foreign = create_subject(&app, &w.teacher, &other_course, "optics").await;
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "x", "subject_id": foreign, "due_at": now + 3_600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Every assigned student must be enrolled…
    let mehmet = login(&app, "mehmet").await;
    let mehmet_id = me_id(&app, &mehmet).await;
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "x", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [mehmet_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // …and the subset caps at 200 names (checked before enrollment, so fakes
    // trip it without 201 signups).
    let fakes: Vec<String> = (0..201).map(|i| format!("01FAKE{i:020}")).collect();
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "x", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": fakes }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Students don't assign homework.
    let res = common::create_homework_with(
        &app,
        &w.ali,
        &w.course,
        json!({ "title": "x", "subject_id": w.subject, "due_at": now + 3_600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Within the grace counts as "due now" — accepted, like a plain future one.
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "just now", "subject_id": w.subject, "due_at": now - 30_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "ch. 3",
        now + 3_600_000,
    )
    .await;
    let res = send(&app, "GET", &format!("/homework/{hw}"), Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "ch. 3");
    assert_eq!(res.body["subject"], w.subject.as_str());
    assert!(
        res.body["assigned"].is_null(),
        "whole-course stores no subset"
    );
}

/// A subset assignment must never leak to the students it leaves out (404s,
/// list omissions), off-course teachers are walled off, the cross-course list
/// narrows per caller, and staff never submit.
#[tokio::test]
async fn homework_subset_hides_from_outsiders() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "secret drill", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = id_of(&res.body);

    // ali is named: both lists carry it and the fetch works.
    let uri = format!("/courses/{}/homework", w.course);
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(&app, "GET", "/homework", Some(&w.ali), None).await;
    assert_eq!(common::total(&res.body), 1);
    // veli is enrolled but unnamed: the homework must not even seem to exist.
    let res = send(&app, "GET", &uri, Some(&w.veli), None).await;
    assert_eq!(
        common::total(&res.body),
        0,
        "unnamed student sees no subset"
    );
    let res = send(&app, "GET", "/homework", Some(&w.veli), None).await;
    assert_eq!(common::total(&res.body), 0);
    let res = send(&app, "GET", &format!("/homework/{hw}"), Some(&w.veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "404, never a 403 leak");
    let res = submit_hw(&app, &w.veli, &hw, Some("hi")).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A teacher without management rights over the course is walled off.
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let res = send(&app, "GET", &format!("/homework/{hw}"), Some(&rival), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&rival),
        Some(json!({ "title": "hijack" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submissions"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = grade_hw(&app, &rival, &hw, &w.ali_id, "done", None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The cross-course list narrows per caller: manager+ everything, an
    // off-course teacher nothing, the managing teacher their course's rows.
    let boss = login_as(&app, &db, "boss", "manager").await;
    let res = send(&app, "GET", "/homework", Some(&boss), None).await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(&app, "GET", "/homework", Some(&rival), None).await;
    assert_eq!(common::total(&res.body), 0);
    let res = send(&app, "GET", "/homework", Some(&w.teacher), None).await;
    assert_eq!(common::total(&res.body), 1);

    // Staff never submit — a 403 on the live role, whatever they can see.
    let res = submit_hw(&app, &boss, &hw, Some("i am staff")).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = submit_hw(&app, &w.teacher, &hw, Some("me neither")).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

/// PATCH re-checks a newly set due date and subject, refuses a narrowing that
/// would orphan existing work (naming the blockers), and leaves stored past
/// due dates alone on unrelated edits.
#[tokio::test]
async fn homework_patch_rechecks_and_blocks_orphaning() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "drill", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id, w.veli_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = id_of(&res.body);

    // A newly set due date must not be past…
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "due_at": now - 120_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // …and a new subject must be the course's own.
    let other_course = create_course(&app, &w.teacher, "physics").await;
    let foreign = create_subject(&app, &w.teacher, &other_course, "optics").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "subject_id": foreign })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let algebra2 = create_subject(&app, &w.teacher, &w.course, "algebra II").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "subject_id": algebra2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], algebra2.as_str());

    // ali submits; narrowing to veli alone would strand that work — refused,
    // naming the blocker. Narrowing to exactly ali passes.
    let res = submit_hw(&app, &w.ali, &hw, Some("done")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "assigned": [w.veli_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"].as_str().unwrap().contains(&w.ali_id),
        "the 409 names whose work blocks: {}",
        res.body
    );
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "assigned": [w.ali_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Widening back to the whole course always passes.
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "assigned": [] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["assigned"].is_null());

    // A title-only edit on a graced, already-due homework keeps its stored
    // due date without tripping the not-past check.
    let old = create_homework(&app, &w.teacher, &w.course, &w.subject, "old", now - 30_000).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{old}"),
        Some(&w.teacher),
        Some(json!({ "title": "renamed" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// Submitting creates once (201) and edits in place after (200): the
/// first-hand-in stamp is pinned, text is replaced whole (omitting clears),
/// and withdrawal removes it all.
#[tokio::test]
async fn homework_submission_lifecycle_and_stamps() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "essay",
        now + 3_600_000,
    )
    .await;

    let uri = format!("/homework/{hw}/submission");
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "nothing submitted yet");

    let res = submit_hw(&app, &w.ali, &hw, Some("draft one")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["text"], "draft one");
    assert_eq!(res.body["late"], false);
    let submitted_at = res.body["submitted_at"].as_i64().unwrap();

    let res = submit_hw(&app, &w.ali, &hw, Some("draft two")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["text"], "draft two");
    assert_eq!(
        res.body["submitted_at"].as_i64().unwrap(),
        submitted_at,
        "the first-submit stamp never moves"
    );
    assert!(res.body["updated_at"].as_i64().unwrap() >= submitted_at);

    // The text rides whole in each submit: omitting it clears.
    let res = submit_hw(&app, &w.ali, &hw, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["text"].is_null());

    // Reads are strictly own: veli asking gets veli's (absent) submission.
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["result"].is_null());
    let res = send(&app, "GET", &uri, Some(&w.veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Withdrawal, twice: gone, then nothing left to withdraw.
    let res = send(&app, "DELETE", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(&app, "DELETE", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// The late flag is computed against `due_at` on every read, and the roster
/// computes `missing` for unsubmitted-past-due rows — no sleeping: a graced
/// creation plants the due date ~30s in the past.
#[tokio::test]
async fn homework_lateness_is_computed_across_due() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let overdue = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "late one",
        now - 30_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &overdue, Some("sorry")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["late"], true, "touched after due_at");

    let open = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "open one",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &open, Some("early")).await;
    assert_eq!(res.body["late"], false);

    // The roster mirrors the flag and derives `missing` for veli, who never
    // submitted — a computation, not a teacher verdict.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{overdue}/submissions"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 2);
    let rows = common::items(&res.body);
    let ali_row = rows
        .iter()
        .find(|row| row["user"] == w.ali_id.as_str())
        .unwrap();
    assert_eq!(ali_row["submission"]["late"], true);
    assert_eq!(ali_row["missing"], false);
    assert_eq!(ali_row["unenrolled"], false);
    let veli_row = rows
        .iter()
        .find(|row| row["user"] == w.veli_id.as_str())
        .unwrap();
    assert!(veli_row["submission"].is_null());
    assert_eq!(veli_row["missing"], true, "unsubmitted past due");
}

/// Grading walls: students never grade, nobody grades themselves, and the
/// target must exist, hold the student role, be enrolled, and be in the
/// audience; the verdict set is closed and the mark bounded. A regrade
/// overwrites its one row, and the student reads the verdict back.
#[tokio::test]
async fn homework_grading_gates_and_bounds() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "drill", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = id_of(&res.body);
    let teacher_id = me_id(&app, &w.teacher).await;

    let res = grade_hw(&app, &w.ali, &hw, &w.ali_id, "done", None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "students never grade");
    let res = grade_hw(&app, &w.teacher, &hw, &teacher_id, "done", None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "no self-grading");
    let res = grade_hw(
        &app,
        &w.teacher,
        &hw,
        "01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        "done",
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "target must exist");
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let rival_id = me_id(&app, &rival).await;
    let res = grade_hw(&app, &w.teacher, &hw, &rival_id, "done", None).await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "only students carry grades"
    );
    let mehmet = login(&app, "mehmet").await;
    let mehmet_id = me_id(&app, &mehmet).await;
    let res = grade_hw(&app, &w.teacher, &hw, &mehmet_id, "done", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "must be enrolled");
    let res = grade_hw(&app, &w.teacher, &hw, &w.veli_id, "done", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "outside the audience");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "late", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "junk status");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(101)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "mark over 100");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(-1)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "negative mark");

    // 200 upsert semantics: the regrade overwrites, never stacks.
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "incomplete", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "incomplete");
    assert!(res.body["mark"].is_null());
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(85)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["mark"], 85);
    assert_eq!(hw_row_count(&db, "homework_result").await, 1);

    // The student reads their own verdict; ungraded means 404, for anyone.
    let uri = format!("/homework/{hw}/result");
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "done");
    assert_eq!(res.body["mark"], 85);
    let res = send(&app, "GET", &uri, Some(&w.veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// A stored grade freezes the submission — text, files, withdrawal — until
/// the teacher removes it; and work never handed in can be graded `missing`,
/// a verdict the student reads from `/result` despite having no submission.
#[tokio::test]
async fn homework_grade_freezes_until_removed() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "essay",
        now + 3_600_000,
    )
    .await;

    let res = submit_hw(&app, &w.ali, &hw, Some("v1")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let up = upload_hw_file(&app, &w.ali, &hw, "draft.txt", "text/plain", b"notes").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let fid = id_of(&up.body);

    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "incomplete", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = submit_hw(&app, &w.ali, &hw, Some("v2")).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "text frozen");
    let res = upload_hw_file(&app, &w.ali, &hw, "late.txt", "text/plain", b"more").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "file add frozen");
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}/submission/files/{fid}"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "file delete frozen");
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "withdrawal frozen");
    // The grade rides on the student's own submission read.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.body["result"]["status"], "incomplete");

    // Un-grading reopens the work; a second removal finds nothing.
    let uri = format!("/homework/{hw}/results/{}", w.ali_id);
    let res = send(&app, "DELETE", &uri, Some(&w.teacher), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = submit_hw(&app, &w.ali, &hw, Some("v2")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "DELETE", &uri, Some(&w.teacher), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Never-submitted work still takes a `missing` verdict, and `/result` is
    // where its student reads it — the submission endpoints have nothing.
    let res = grade_hw(&app, &w.teacher, &hw, &w.veli_id, "missing", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/result"),
        Some(&w.veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "missing");
}

/// Files round-trip: a photo-first upload auto-creates the submission, the
/// owner and the managing teacher download the exact bytes as a forced
/// attachment, file ids are scoped to their homework, other students see
/// nothing, and a delete unlinks the blob.
#[tokio::test]
async fn homework_files_roundtrip_and_scope() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "photo hw",
        now + 3_600_000,
    )
    .await;

    let bytes = b"%PDF-1.4 homework \x00\x01".to_vec();
    let up = upload_hw_file(&app, &w.ali, &hw, "odev.pdf", "application/pdf", &bytes).await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(up.body["name"], "odev.pdf");
    assert_eq!(up.body["content_type"], "application/pdf");
    assert_eq!(up.body["size"], bytes.len() as i64);
    let fid = id_of(&up.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "photo-first auto-creates");
    assert!(res.body["text"].is_null());
    assert_eq!(res.body["files"].as_array().unwrap().len(), 1);

    // Owner and managing teacher read the same bytes — always an attachment:
    // homework files take any content type, so inline HTML/SVG would be XSS.
    let file_uri = format!("/homework/{hw}/submission/files/{fid}");
    for viewer in [&w.ali, &w.teacher] {
        let (status, headers, body) =
            common::send_raw(&app, "GET", &file_uri, Some(viewer), None, Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, bytes);
        assert_eq!(headers["content-type"], "application/pdf");
        assert_eq!(
            headers["content-disposition"],
            "attachment; filename=\"odev.pdf\"; filename*=UTF-8''odev.pdf"
        );
    }
    // The id only resolves under its own homework, and never for a classmate.
    let other = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "other",
        now + 3_600_000,
    )
    .await;
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/homework/{other}/submission/files/{fid}"),
        Some(&w.teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "scoped to its homework");
    let (status, _, _) =
        common::send_raw(&app, "GET", &file_uri, Some(&w.veli), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "classmates see nothing");

    // Delete removes the row and unlinks the blob.
    let keys = homework_blob_keys(&db).await;
    assert_eq!(keys.len(), 1);
    let res = send(&app, "DELETE", &file_uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        !common::files_dir().join(&keys[0]).exists(),
        "blob must be unlinked with its row"
    );
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.body["files"].as_array().unwrap().len(), 0);
}

/// File limits: empty uploads are rejected, the 11th file trips the cap, and
/// the school's `max_file_bytes` bounds each file (413 over, 201 at).
#[tokio::test]
async fn homework_file_limits_follow_cap_and_settings() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "many files",
        now + 3_600_000,
    )
    .await;

    let res = upload_hw_file(&app, &w.ali, &hw, "empty.txt", "text/plain", b"").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    for i in 0..10 {
        let res = upload_hw_file(&app, &w.ali, &hw, &format!("f{i}.txt"), "text/plain", b"x").await;
        assert_eq!(res.status, StatusCode::CREATED, "file {i}: {}", res.body);
    }
    let res = upload_hw_file(&app, &w.ali, &hw, "f10.txt", "text/plain", b"x").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(
        homework_blob_keys(&db).await.len(),
        10,
        "the losing upload leaves no row"
    );

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "max_file_bytes": 1024 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let hw2 = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "sized",
        now + 3_600_000,
    )
    .await;
    let res = upload_hw_file(&app, &w.ali, &hw2, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(res.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", res.body);
    let res = upload_hw_file(&app, &w.ali, &hw2, "fits.bin", "", &vec![7u8; 1024]).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["content_type"], "application/octet-stream");
}

/// The roster carries the whole audience — and keeps a straggler's work
/// visible (flagged `unenrolled`) instead of letting it vanish; a subset
/// roster lists the named students only; the list pages.
#[tokio::test]
async fn homework_roster_covers_audience_and_stragglers() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "for all",
        now + 3_600_000,
    )
    .await;
    let roster_uri = format!("/homework/{hw}/submissions");
    let res = send(&app, "GET", &roster_uri, Some(&w.teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 2, "whole course, both enrolled");

    // ali submits, then leaves the course: the work stays on the roster,
    // flagged — and grading the straggler is refused.
    let res = submit_hw(&app, &w.ali, &hw, Some("mine")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{}/enrollments/{}", w.course, w.ali_id),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &roster_uri, Some(&w.teacher), None).await;
    assert_eq!(common::total(&res.body), 2, "the straggler's work stays");
    let rows = common::items(&res.body);
    let ali_row = rows
        .iter()
        .find(|row| row["user"] == w.ali_id.as_str())
        .unwrap();
    assert_eq!(ali_row["unenrolled"], true);
    assert_eq!(ali_row["submission"]["text"], "mine");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", None).await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "no grading the unenrolled"
    );

    // The roster pages like every list.
    let res = send(
        &app,
        "GET",
        &format!("{roster_uri}?limit=1&offset=1"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(res.body["limit"], 1);
    assert_eq!(res.body["offset"], 1);

    // A subset roster is the named students, nobody else.
    enroll(&app, &w.teacher, &w.course, &w.ali_id).await;
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "subset", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let subset_hw = id_of(&res.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{subset_hw}/submissions"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(common::items(&res.body)[0]["user"], w.ali_id.as_str());
}

/// The observer report: full for manager+ and a linked parent, narrowed to
/// managed courses for a teacher, walled for everyone else — and it carries
/// statuses, marks, and flags, never file bytes.
#[tokio::test]
async fn homework_report_serves_observers_only() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let now = Timestamp::now().as_millis();

    // A second course under another teacher; ali sits in both.
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let course2 = create_course(&app, &rival, "physics").await;
    let subject2 = create_subject(&app, &rival, &course2, "optics").await;
    enroll(&app, &rival, &course2, &w.ali_id).await;

    // Three states across the two courses: graded, submitted-late, missing.
    let graded = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "graded",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &graded, Some("done")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = grade_hw(&app, &w.teacher, &graded, &w.ali_id, "done", Some(90)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let overdue = create_homework(&app, &rival, &course2, &subject2, "overdue", now - 30_000).await;
    let res = submit_hw(&app, &w.ali, &overdue, Some("late")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let missing = create_homework(&app, &rival, &course2, &subject2, "skipped", now - 30_000).await;

    // The admin reads all three rows, each in its computed state.
    let report_uri = format!("/homework/report/{}", w.ali_id);
    let res = send(&app, "GET", &report_uri, Some(&admin), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 3);
    let rows = common::items(&res.body);
    let graded_row = rows
        .iter()
        .find(|row| row["homework"] == graded.as_str())
        .unwrap();
    assert_eq!(graded_row["submitted"], true);
    assert_eq!(graded_row["late"], false);
    assert_eq!(graded_row["result"]["status"], "done");
    assert_eq!(graded_row["result"]["mark"], 90);
    let late_row = rows
        .iter()
        .find(|row| row["homework"] == overdue.as_str())
        .unwrap();
    assert_eq!(late_row["late"], true);
    assert_eq!(late_row["missing"], false);
    assert!(late_row["result"].is_null());
    let missing_row = rows
        .iter()
        .find(|row| row["homework"] == missing.as_str())
        .unwrap();
    assert_eq!(missing_row["submitted"], false);
    assert_eq!(missing_row["missing"], true);

    // A teacher reads only the slice they manage.
    let res = send(&app, "GET", &report_uri, Some(&w.teacher), None).await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(common::items(&res.body)[0]["course"], w.course.as_str());

    // A linked parent reads all of it (paged like every list)…
    let mom = login_as(&app, &db, "mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": w.ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", &report_uri, Some(&mom), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 3);
    let res = send(
        &app,
        "GET",
        &format!("{report_uri}?limit=2&offset=2"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(common::items(&res.body).len(), 1);

    // …but never the underlying work: no submission reads, no file bytes.
    let up = upload_hw_file(&app, &w.ali, &overdue, "p.png", "image/png", b"png").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let fid = id_of(&up.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{overdue}/submission"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/homework/{overdue}/submission/files/{fid}"),
        Some(&mom),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "reports, never bytes");

    // Unlinked parent, the student themselves, and a ghost target all fail.
    let dad = login_as(&app, &db, "dad", "parent").await;
    let res = send(&app, "GET", &report_uri, Some(&dad), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "unlinked parent");
    let res = send(&app, "GET", &report_uri, Some(&w.ali), None).await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "self-reads use /homework"
    );
    let res = send(
        &app,
        "GET",
        "/homework/report/01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// Every delete path collects its garbage: withdrawing a submission, deleting
/// a homework, and deleting the whole course each remove the database rows
/// *and* the blobs on disk.
#[tokio::test]
async fn homework_gc_removes_rows_and_blobs() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let hw1 = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "one",
        now + 3_600_000,
    )
    .await;
    let hw2 = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "two",
        now + 3_600_000,
    )
    .await;
    for (cookie, hw, name) in [
        (&w.ali, &hw1, "a.txt"),
        (&w.veli, &hw1, "b.txt"),
        (&w.ali, &hw2, "c.txt"),
    ] {
        let res = upload_hw_file(&app, cookie, hw, name, "text/plain", b"data").await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    let res = grade_hw(&app, &w.teacher, &hw1, &w.veli_id, "done", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let keys = homework_blob_keys(&db).await;
    assert_eq!(keys.len(), 3);
    for key in &keys {
        assert!(common::files_dir().join(key).exists(), "blob {key} on disk");
    }

    // Withdrawing a submission takes its file rows and blobs.
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw2}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let remaining = homework_blob_keys(&db).await;
    assert_eq!(remaining.len(), 2);
    for gone in keys.iter().filter(|key| !remaining.contains(key)) {
        assert!(
            !common::files_dir().join(gone).exists(),
            "withdrawn blob {gone} must be unlinked"
        );
    }

    // Deleting the homework cascades submissions, files, and grades — blobs
    // included.
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw1}"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(hw_row_count(&db, "homework_submission").await, 0);
    assert_eq!(hw_row_count(&db, "homework_file").await, 0);
    assert_eq!(hw_row_count(&db, "homework_result").await, 0);
    assert_eq!(hw_row_count(&db, "homework").await, 1, "hw2 remains");
    for key in &remaining {
        assert!(
            !common::files_dir().join(key).exists(),
            "blob {key} must die with the homework"
        );
    }

    // Deleting the course takes everything left.
    let res = upload_hw_file(&app, &w.ali, &hw2, "d.txt", "text/plain", b"data").await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = grade_hw(&app, &w.teacher, &hw2, &w.veli_id, "missing", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let keys = homework_blob_keys(&db).await;
    assert_eq!(keys.len(), 1);
    unenroll(&app, &w.teacher, &w.course, &w.ali_id).await;
    unenroll(&app, &w.teacher, &w.course, &w.veli_id).await;
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{}", w.course),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    for table in [
        "homework",
        "homework_submission",
        "homework_file",
        "homework_result",
    ] {
        assert_eq!(
            hw_row_count(&db, table).await,
            0,
            "{table} died with course"
        );
    }
    for key in &keys {
        assert!(
            !common::files_dir().join(key).exists(),
            "blob {key} must die with the course"
        );
    }
}

/// A promotion sweeps the enrollment but never the homework rows; the gates
/// re-read the live role and deny, and the roster keeps the stale work
/// visible, flagged `unenrolled`.
#[tokio::test]
async fn homework_rows_survive_promotion_but_gates_deny() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "before",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &hw, Some("as a student")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(70)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{}/role", w.ali_id),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(hw_row_count(&db, "homework_submission").await, 1);
    assert_eq!(hw_row_count(&db, "homework_result").await, 1);

    // Live-role gates: no submission access, no fresh grades onto staff.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = submit_hw(&app, &w.ali, &hw, Some("as staff")).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "no longer a student");

    // The roster keeps the stale work, flagged (the promotion swept the
    // enrollment).
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submissions"),
        Some(&w.teacher),
        None,
    )
    .await;
    let rows = common::items(&res.body);
    let row = rows
        .iter()
        .find(|row| row["user"] == w.ali_id.as_str())
        .unwrap();
    assert_eq!(row["unenrolled"], true);
    assert_eq!(row["result"]["mark"], 70);
}

/// A subject with homework can't be deleted (409 naming homework) until the
/// homework is re-tagged or removed — the guard the exam questions already
/// have.
#[tokio::test]
async fn homework_blocks_subject_delete_until_retagged() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "tagged",
        now + 3_600_000,
    )
    .await;

    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{}", w.subject),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"].as_str().unwrap().contains("homework"),
        "{}",
        res.body
    );

    // Re-tagging frees the old subject and guards the new one.
    let second = create_subject(&app, &w.teacher, &w.course, "geometry").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "subject_id": second })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{}", w.subject),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{second}"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
}

/// The two homework lists page through the standard envelope.
#[tokio::test]
async fn homework_lists_paginate() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    for i in 0..3 {
        create_homework(
            &app,
            &w.teacher,
            &w.course,
            &w.subject,
            &format!("hw {i}"),
            now + 3_600_000,
        )
        .await;
    }
    for uri in [
        format!("/courses/{}/homework?limit=2&offset=2", w.course),
        "/homework?limit=2&offset=2".to_string(),
    ] {
        let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}");
        assert_eq!(common::total(&res.body), 3, "{uri}");
        assert_eq!(common::items(&res.body).len(), 1, "{uri}");
        assert_eq!(res.body["limit"], 2);
        assert_eq!(res.body["offset"], 2);
    }
}

/// A course carrying students can't be deleted (409) until the roster is
/// emptied — the cascade would otherwise take the enrollments with it.
#[tokio::test]
async fn course_delete_blocks_while_students_are_enrolled() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "cdb_t", "teacher").await;
    let student = login(&app, "cdb_s").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "Chemistry").await;
    enroll(&app, &teacher, &course, &student_id).await;

    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Empty the roster: the delete goes through.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{student_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// An exam kind whose exams already carry marks can't leave the settings list
/// (409) — weights are read live, so those marks would silently re-weight.
/// Ungraded kinds still leave freely.
#[tokio::test]
async fn exam_kind_removal_blocks_while_marks_exist() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ekd_t", "teacher").await;
    let manager = login_as(&app, &db, "ekd_m", "manager").await;
    let student = login(&app, "ekd_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "Biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = create_exam(&app, &teacher, &course, "Final", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "user_id": student_id, "mark": 70 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Dropping the graded kind is refused; the list is untouched.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{ "name": "quiz", "weight": 1 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(&app, "GET", "/settings", Some(&manager), None).await;
    let kinds: Vec<String> = res.body["exam_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["name"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"final".to_string()), "{}", res.body);
    assert!(kinds.contains(&"quiz".to_string()), "{}", res.body);

    // Keeping the graded kind, dropping the ungraded ones, is fine.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{ "name": "final", "weight": 5 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["exam_kinds"].as_array().unwrap().len(), 1);
    assert_eq!(res.body["exam_kinds"][0]["weight"], 5);
}

// --- chatbot relay -------------------------------------------------------
//
// What the `POST` itself decides: the receipt, the availability gate, the
// limits, and who may see a thread. The bridge round trip and the answer that
// comes back are `ai_bridge.rs`'s job — the fake service dialled in here
// completes the handshake and then stays quiet on purpose, so nothing in this
// file depends on an answer arriving.

use hezarfen_backend::ai::protocol::{Greeting, Hello, read_frame, write_frame};
use hezarfen_backend::ai::{AiBridge, BridgeConfig};
use hezarfen_backend::constant::{AI_ALPN, AI_CHAT_CAPABILITY, AI_PROTOCOL};
use hezarfen_backend::database::Database;

const CHAT_TOKEN: &str = "integration-ai-token";

/// A bridge with one registered — but silent — `chat.reply` service. Holding
/// it keeps the registration alive: the control stream's closure is a goodbye.
struct ChatBridge {
    bridge: AiBridge,
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
    _control: (quinn::SendStream, quinn::RecvStream),
}

async fn chat_bridge() -> ChatBridge {
    let bridge = AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: CHAT_TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: std::time::Duration::from_secs(5),
    })
    .await
    .expect("bridge binds on an ephemeral port");

    hezarfen_backend::ai::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(bridge.certificate()).expect("pin the leaf");
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];
    let config = quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC-usable TLS"),
    ));
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
    endpoint.set_default_client_config(config);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .expect("dial")
        .await
        .expect("QUIC handshake");

    let (mut send_stream, mut recv_stream) = conn.open_bi().await.expect("control stream");
    write_frame(
        &mut send_stream,
        &Hello {
            protocol: AI_PROTOCOL.to_string(),
            service: "fake-chat".to_string(),
            capabilities: vec![AI_CHAT_CAPABILITY.to_string()],
            token: CHAT_TOKEN.to_string(),
            max_concurrent: None,
        },
    )
    .await
    .expect("send Hello");
    let greeting: Greeting = read_frame(&mut recv_stream).await.expect("read Greeting");
    assert!(matches!(greeting, Greeting::Welcome { .. }), "{greeting:?}");

    // Registration completes after the welcome is on the wire, so wait for it
    // rather than racing it.
    for _ in 0..300 {
        if bridge.has_capability(AI_CHAT_CAPABILITY) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        bridge.has_capability(AI_CHAT_CAPABILITY),
        "service never registered"
    );

    ChatBridge {
        bridge,
        _endpoint: endpoint,
        _conn: conn,
        _control: (send_stream, recv_stream),
    }
}

/// A router whose AI bridge is `ai`, plus a handle to its database.
async fn chat_app(ai: Option<AiBridge>) -> (axum::Router, Database) {
    chat_app_limited(ai, Default::default()).await
}

/// The same, with an explicit per-user chat tier — what
/// `RATE_LIMIT_CHATBOT_PER_MINUTE` configures in production.
async fn chat_app_limited(
    ai: Option<AiBridge>,
    chatbot_limit: hezarfen_backend::rate_limit::UserRateLimiter,
) -> (axum::Router, Database) {
    let db = database::init_mem().await.expect("in-memory db");
    let app = build_router(AppState {
        db: db.clone(),
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit,
        exam_presence: Default::default(),
        db_up: Default::default(),
        ai,
    });
    (app, db)
}

/// How many `thread` and `chatbot_message` rows exist, anywhere.
async fn chat_rows(db: &Database) -> (usize, usize) {
    let mut res = db
        .query("SELECT VALUE id FROM chatbot_thread; SELECT VALUE id FROM chatbot_message;")
        .await
        .expect("count query")
        .check()
        .expect("count check");
    let threads: Vec<surrealdb::types::RecordId> = res.take(0).expect("thread ids");
    let messages: Vec<surrealdb::types::RecordId> = res.take(1).expect("chatbot_message ids");
    (threads.len(), messages.len())
}

/// Open a thread (asserts 201) and return its id.
async fn new_thread(app: &axum::Router, cookie: &str) -> String {
    let res = send(
        app,
        "POST",
        "/chatbot/threads",
        Some(cookie),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

async fn post_turn(app: &axum::Router, cookie: &str, thread: &str, content: &str) -> common::Res {
    send(
        app,
        "POST",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(cookie),
        Some(json!({ "content": content })),
    )
    .await
}

/// Open the SSE stream without draining it — a `pending` turn's stream stays
/// open, so reading its body here would hang.
async fn open_stream(app: &axum::Router, cookie: &str, thread: &str, mid: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/chatbot/threads/{thread}/messages/{mid}/stream"))
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn chat_send_is_accepted_with_a_reserved_assistant_row() {
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let cookie = login(&app, "ali").await;

    let res = send(
        &app,
        "POST",
        "/chatbot/threads",
        Some(&cookie),
        Some(json!({ "title": "Fizik" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["title"], "Fizik");
    assert!(res.body["created_at"].is_i64(), "{}", res.body);
    assert!(res.body["updated_at"].is_i64(), "{}", res.body);
    let thread = id_of(&res.body);

    // The receipt: the reserved row's id and nothing else. The answer is not
    // here — it is being fetched.
    let res = post_turn(&app, &cookie, &thread, "ikinci yasa nedir?").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    let mid = res.body["message_id"].as_str().expect("message_id");
    assert!(!mid.is_empty());
    assert_eq!(
        res.body.as_object().expect("object body").len(),
        2,
        "{}",
        res.body
    );

    // Both rows are already durable, and the reserved one reads back empty.
    assert_eq!(chat_rows(&db).await, (1, 2));
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{thread}/messages/{mid}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["thread_id"], thread);
    assert_eq!(res.body["role"], "assistant");
    assert_eq!(res.body["status"], "pending");
    assert_eq!(res.body["content"], "");
    assert!(res.body["error_code"].is_null(), "{}", res.body);
    assert!(res.body["completed_at"].is_null(), "{}", res.body);
    assert!(res.body["created_at"].is_i64(), "{}", res.body);

    // The thread holds the question and its reserved answer, in that order —
    // the two rows one POST writes share a millisecond, so this is the id
    // tie-break of the route's `ORDER BY` doing its job.
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let turns = common::items(&res.body);
    assert_eq!(turns.len(), 2, "{}", res.body);
    assert_eq!(turns[0]["role"], "user", "{}", res.body);
    assert_eq!(turns[0]["content"], "ikinci yasa nedir?", "{}", res.body);
    assert_eq!(turns[1]["role"], "assistant", "{}", res.body);
    assert_eq!(turns[1]["id"], mid, "{}", res.body);
}

#[tokio::test]
async fn chat_send_without_a_service_is_503_and_writes_nothing() {
    // Both ways the gate can say no: no bridge on this deployment at all, and
    // a bridge with nothing declaring `chat.reply`. Neither may leave a user
    // turn stranded next to a pending row nothing could ever answer.
    let idle = AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: CHAT_TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: std::time::Duration::from_secs(5),
    })
    .await
    .expect("bridge binds");

    for ai in [None, Some(idle)] {
        let (app, db) = chat_app(ai).await;
        let cookie = login(&app, "ali").await;
        let thread = new_thread(&app, &cookie).await;

        let res = post_turn(&app, &cookie, &thread, "bir soru").await;
        assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
        assert!(
            res.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("AI"),
            "{}",
            res.body
        );
        // The thread exists (it was created), but the turn left no trace: no
        // user message, no pending assistant row.
        assert_eq!(chat_rows(&db).await, (1, 0), "a refused turn wrote rows");
    }
}

#[tokio::test]
async fn chat_lists_are_paged() {
    let ai = chat_bridge().await;
    let (app, _db) = chat_app(Some(ai.bridge.clone())).await;
    let cookie = login(&app, "ali").await;

    // Three threads, and three turns (six rows) inside the first.
    let mut threads = Vec::new();
    for _ in 0..3 {
        threads.push(new_thread(&app, &cookie).await);
    }
    for n in 0..3 {
        let res = post_turn(&app, &cookie, &threads[0], &format!("soru {n}")).await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    }

    // Both routes answer the same envelope, and the pages partition the whole
    // list. Order is compared element for element: the id tie-break makes it
    // deterministic even for the two rows one POST writes in a millisecond.
    for (uri, expected) in [
        ("/chatbot/threads".to_string(), 3),
        (format!("/chatbot/threads/{}/messages", threads[0]), 6),
    ] {
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=2&offset=0"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "{}", res.body);
        assert_eq!(res.body["limit"], 2, "{}", res.body);
        assert_eq!(res.body["offset"], 0, "{}", res.body);
        let mut ids: Vec<String> = common::items(&res.body).iter().map(id_of).collect();
        assert_eq!(ids.len(), 2);

        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=99&offset=2"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "{}", res.body);
        assert_eq!(res.body["offset"], 2, "{}", res.body);
        ids.extend(common::items(&res.body).iter().map(id_of));

        // Paging is a window on one stable order, not a reshuffle: the pages
        // concatenated must reproduce the unpaged list exactly.
        let res = send(&app, "GET", &uri, Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let whole: Vec<String> = common::items(&res.body).iter().map(id_of).collect();
        assert_eq!(ids, whole, "the pages must reproduce {uri} in order");

        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len() as i64,
            expected,
            "the pages must partition {uri}, not overlap or skip"
        );

        // Past the end is an empty page, not an error.
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=5&offset=999"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert!(common::items(&res.body).is_empty(), "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "{}", res.body);
    }
}

#[tokio::test]
async fn chat_threads_are_invisible_to_everyone_else() {
    // Nobody reads someone else's thread — not a teacher, not an admin. A
    // foreign id is a 404, never a 403: its existence is not leaked either.
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    let res = post_turn(&app, &ali, &thread, "bir soru").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    let mid = res.body["message_id"].as_str().unwrap().to_string();

    for (name, role) in [
        ("veli", "student"),
        ("hoca", "teacher"),
        ("patron", "admin"),
    ] {
        let other = login_as(&app, &db, name, role).await;
        for (method, uri) in [
            ("GET", format!("/chatbot/threads/{thread}/messages")),
            ("GET", format!("/chatbot/threads/{thread}/messages/{mid}")),
            (
                "GET",
                format!("/chatbot/threads/{thread}/messages/{mid}/stream"),
            ),
            ("DELETE", format!("/chatbot/threads/{thread}")),
            ("PATCH", format!("/chatbot/threads/{thread}")),
            ("POST", format!("/chatbot/threads/{thread}/messages")),
        ] {
            let body = match method {
                "POST" => Some(json!({ "content": "sızıntı" })),
                "PATCH" => Some(json!({ "title": "benim artık" })),
                _ => None,
            };
            let res = send(&app, method, &uri, Some(&other), body).await;
            assert_eq!(res.status, StatusCode::NOT_FOUND, "{name} {method} {uri}");
        }
        // Nor does someone else's thread show up in their own list.
        let res = send(&app, "GET", "/chatbot/threads", Some(&other), None).await;
        assert!(common::items(&res.body).is_empty(), "{}", res.body);
    }

    // And the owner still has everything, untouched.
    assert_eq!(chat_rows(&db).await, (1, 2));
    let res = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    assert_eq!(common::items(&res.body).len(), 1, "{}", res.body);
}

#[tokio::test]
async fn chat_thread_count_is_capped_by_the_school() {
    let (app, db) = chat_app(None).await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_threads": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let ali = login(&app, "ali").await;
    let first = new_thread(&app, &ali).await;
    let res = send(
        &app,
        "POST",
        "/chatbot/threads",
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // The cap is storage protection, per user: another user is unaffected, and
    // deleting a thread frees the slot.
    let veli = login(&app, "veli").await;
    let _ = new_thread(&app, &veli).await;

    let res = send(
        &app,
        "DELETE",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let _ = new_thread(&app, &ali).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_thread_creation_never_passes_the_cap() {
    // The sequential test above cannot see this: count-then-create is only a
    // cap if something serializes the two, and the database's transactions do
    // not serialize a count against concurrent inserts. Sixteen simultaneous
    // creators against a cap of five must still leave five rows.
    const CAP: usize = 5;
    const RACERS: usize = 16;

    let (app, db) = chat_app(None).await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_threads": CAP })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let ali = login(&app, "ali").await;

    // A barrier, so every request is inside the check-then-write window at
    // once rather than trickling in.
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (app, cookie, gate) = (app.clone(), ali.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                send(
                    &app,
                    "POST",
                    "/chatbot/threads",
                    Some(&cookie),
                    Some(json!({})),
                )
                .await
                .status
            })
        })
        .collect();

    let mut created = 0;
    for racer in racers {
        match racer.await.expect("no panic in a racer") {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(created, CAP, "the cap was over-admitted");
    assert_eq!(
        chat_rows(&db).await.0,
        CAP,
        "rows past the cap were written"
    );
}

#[tokio::test]
async fn chat_turn_stamps_thread_activity() {
    // The `updated_at` bump is best-effort (a failure must never strand the
    // pending row it follows) — but it still has to happen.
    let ai = chat_bridge().await;
    let (app, _db) = chat_app(Some(ai.bridge.clone())).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    let before = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let opened = common::items(&before.body)[0]["updated_at"]
        .as_i64()
        .unwrap();

    // The stamp is in whole milliseconds; make sure the clock has moved.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let res = post_turn(&app, &ali, &thread, "bir soru").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);

    let after = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let stamped = common::items(&after.body)[0]["updated_at"]
        .as_i64()
        .unwrap();
    assert!(stamped > opened, "{stamped} !> {opened}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_stops_polling_when_the_client_hangs_up() {
    // A `pending` turn sends nothing, so nothing used to notice a vanished
    // client: the poll task kept re-reading the row every 200ms for the whole
    // 300-second staleness window, and `/stream` is not rate-limited. Dropping
    // the response body is exactly what axum does when a connection dies.
    // No bridge here: the pending row is written straight to the database, so
    // the only task this runtime gains is the stream's own poll loop and the
    // count below is not drowned in the QUIC service's task churn.
    let (app, db) = chat_app(None).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    let me = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    let user = UserId::from_key(me.body["id"].as_str().unwrap());
    let pending =
        ChatbotMessage::append_pending_assistant(&ChatbotThreadId::from_key(&thread), &user, &db)
            .await
            .expect("reserve an assistant row");
    let mid = pending.get_id().key().to_string();

    // The witness is the runtime's task count, so it is read only once the
    // setup's short-lived database tasks have drained — otherwise one of those
    // finishing mid-measurement looks like the stream's own task.
    let metrics = tokio::runtime::Handle::current().metrics();
    let mut before = metrics.num_alive_tasks();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let now = metrics.num_alive_tasks();
        if now == before {
            break;
        }
        before = now;
    }
    let request = Request::builder()
        .method("GET")
        .uri(format!("/chatbot/threads/{thread}/messages/{mid}/stream"))
        .header("cookie", &ali)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        metrics.num_alive_tasks() > before,
        "the stream's poll task should be running"
    );

    drop(response);
    for _ in 0..20 {
        if metrics.num_alive_tasks() <= before {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the stream kept polling after its client hung up");
}

#[tokio::test]
async fn chat_content_is_required_and_capped_by_the_school() {
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_message_len": 100 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;

    for empty in ["", "   ", "\n\t"] {
        let res = post_turn(&app, &ali, &thread, empty).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{empty:?} accepted");
    }
    // Counted in characters, not bytes: 101 multi-byte characters is 101.
    let res = post_turn(&app, &ali, &thread, &"é".repeat(101)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // Nothing rejected was written.
    assert_eq!(chat_rows(&db).await, (1, 0));

    let res = post_turn(&app, &ali, &thread, &"é".repeat(100)).await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
}

#[tokio::test]
async fn chat_rate_limit_refuses_before_anything_is_written() {
    // 20 messages a minute per user (the documented tier). The limiter is
    // charged first, so the refused turn leaves no row behind — and no other
    // user inherits the window.
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;

    for n in 1..=20 {
        let res = post_turn(&app, &ali, &thread, &format!("soru {n}")).await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "turn {n}: {}", res.body);
    }
    let before = chat_rows(&db).await;
    assert_eq!(before, (1, 40));

    let (status, headers, _) = common::send_raw(
        &app,
        "POST",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(&ali),
        Some("application/json"),
        json!({ "content": "yirmi birinci" })
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry_after = headers
        .get("retry-after")
        .expect("Retry-After tells the client when to come back")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .expect("whole seconds");
    assert!((1..=60).contains(&retry_after), "{retry_after}");
    assert_eq!(chat_rows(&db).await, before, "a refused turn wrote rows");

    let veli = login(&app, "veli").await;
    let other = new_thread(&app, &veli).await;
    let res = post_turn(&app, &veli, &other, "benim ilk sorum").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
}

#[tokio::test]
async fn every_role_may_chat_parents_included() {
    // Deliberate, not an oversight: the chatbot is a `CurrentUser` route for
    // every role the school has, parents among them.
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;

    for (name, role) in [
        ("veli", "parent"),
        ("ogrenci", "student"),
        ("hoca", "teacher"),
        ("mudur", "manager"),
        ("patron", "admin"),
    ] {
        let cookie = login_as(&app, &db, name, role).await;
        let thread = new_thread(&app, &cookie).await;

        let res = post_turn(&app, &cookie, &thread, "bir soru").await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "{role}: {}", res.body);
        let mid = res.body["message_id"].as_str().unwrap().to_string();

        let res = send(
            &app,
            "GET",
            &format!("/chatbot/threads/{thread}/messages/{mid}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{role}: {}", res.body);

        let res = send(
            &app,
            "GET",
            &format!("/chatbot/threads/{thread}/messages"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{role}: {}", res.body);
        assert_eq!(common::items(&res.body).len(), 2, "{role}: {}", res.body);

        assert_eq!(
            open_stream(&app, &cookie, &thread, &mid).await,
            StatusCode::OK,
            "{role} may open the stream"
        );

        let res = send(&app, "GET", "/chatbot/threads", Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{role}: {}", res.body);
        assert_eq!(common::items(&res.body).len(), 1, "{role}: {}", res.body);

        let res = send(
            &app,
            "DELETE",
            &format!("/chatbot/threads/{thread}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::NO_CONTENT, "{role}: {}", res.body);
    }
    // Every thread deleted, every turn cascaded with it.
    assert_eq!(chat_rows(&db).await, (0, 0));
}

#[tokio::test]
async fn chat_thread_can_be_renamed_and_cleared() {
    // Renaming is how a user files a thread, so it counts as activity and the
    // thread moves to the top of the list. Nothing renames a thread by itself:
    // there is no auto-titling from the first message.
    let (app, _db) = chat_app(None).await;
    let ali = login(&app, "ali").await;
    let first = new_thread(&app, &ali).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = new_thread(&app, &ali).await;

    let listed = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let items = common::items(&listed.body);
    assert_eq!(id_of(&items[0]), second, "newest activity first");
    let before = items[1]["updated_at"].as_i64().expect("updated_at");
    assert!(items[1]["title"].is_null(), "{}", listed.body);

    // The stamp is in whole milliseconds; make sure the clock has moved.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        Some(json!({ "title": "  Fizik ödevi  " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["title"], "Fizik ödevi", "trimmed, not stored raw");
    assert_eq!(id_of(&res.body), first);
    let stamped = res.body["updated_at"].as_i64().expect("updated_at");
    assert!(stamped > before, "{stamped} !> {before}");

    // The rename is durable and re-sorted the list.
    let listed = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let items = common::items(&listed.body);
    assert_eq!(id_of(&items[0]), first, "the renamed thread is now first");
    assert_eq!(items[0]["title"], "Fizik ödevi");

    // `null` — and a blank string — clear the name back to untitled.
    for clearing in [json!({ "title": null }), json!({ "title": "   " })] {
        let res = send(
            &app,
            "PATCH",
            &format!("/chatbot/threads/{first}"),
            Some(&ali),
            Some(clearing.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{clearing}: {}", res.body);
        assert!(res.body["title"].is_null(), "{clearing}: {}", res.body);

        // Restore a name so the next iteration has something to clear.
        send(
            &app,
            "PATCH",
            &format!("/chatbot/threads/{first}"),
            Some(&ali),
            Some(json!({ "title": "geri" })),
        )
        .await;
    }

    // Too long is a 400, and the stored name is untouched.
    let res = send(
        &app,
        "PATCH",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        Some(json!({ "title": "é".repeat(201) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        Some(json!({ "title": "é".repeat(200) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A thread that never existed is the same 404 a foreign one gets.
    let res = send(
        &app,
        "PATCH",
        "/chatbot/threads/nosuchthread",
        Some(&ali),
        Some(json!({ "title": "hayalet" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // And `second` was never touched by any of it.
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{second}/messages"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn chat_rate_limit_tier_is_configurable_and_zero_disables_it() {
    // `RATE_LIMIT_CHATBOT_PER_MINUTE`, the third tier, behaves like the two IP
    // ones: the configured number is honoured exactly and `0` switches it off.
    // No bridge is needed — the limiter is charged before the availability
    // gate, so a metered turn answers 503 and only the refused one answers 429.
    use hezarfen_backend::rate_limit::UserRateLimiter;

    let (app, _db) = chat_app_limited(None, UserRateLimiter::per_user_minute(3)).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    for n in 1..=3 {
        let res = post_turn(&app, &ali, &thread, &format!("soru {n}")).await;
        assert_eq!(
            res.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "turn {n} was inside the budget: {}",
            res.body
        );
    }
    let res = post_turn(&app, &ali, &thread, "dorduncu").await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS, "{}", res.body);
    // Per user, still: a second caller starts with a full budget.
    let veli = login(&app, "veli").await;
    let other = new_thread(&app, &veli).await;
    assert_eq!(
        post_turn(&app, &veli, &other, "benim ilk sorum")
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // `0` disables the tier: well past the shipped default, nothing is refused.
    let (app, _db) = chat_app_limited(None, UserRateLimiter::per_user_minute(0)).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    for n in 1..=40 {
        let res = post_turn(&app, &ali, &thread, &format!("soru {n}")).await;
        assert_eq!(
            res.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "turn {n} was rate-limited with the tier off: {}",
            res.body
        );
    }
}

// --- appointments ----------------------------------------------------------

const HOUR_MS: i64 = 3_600_000;
const WEEK_MS: i64 = 7 * 24 * HOUR_MS;

/// Publish availability from a full JSON body (no assertion). The response is
/// always an array: one element for a one-off, one per occurrence for a weekly.
async fn publish_slots(app: &axum::Router, cookie: &str, body: serde_json::Value) -> common::Res {
    send(app, "POST", "/appointments/slots", Some(cookie), Some(body)).await
}

/// Publish one slot as `cookie` (asserts 201); returns its id.
async fn publish_slot(app: &axum::Router, cookie: &str, starts_at: i64, ends_at: i64) -> String {
    let res = publish_slots(
        app,
        cookie,
        json!({ "starts_at": starts_at, "ends_at": ends_at }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "publish slot: {}",
        res.body
    );
    res.body[0]["id"].as_str().expect("slot id").to_string()
}

/// Ask for a meeting on `slot` (no assertion).
async fn book_slot(app: &axum::Router, cookie: &str, slot: &str) -> common::Res {
    send(
        app,
        "POST",
        "/appointments",
        Some(cookie),
        Some(json!({ "slot": slot, "reason": "görüşmek istiyorum" })),
    )
    .await
}

/// Book `slot`, asserting it lands pending; returns the booking id.
async fn book_ok(app: &axum::Router, cookie: &str, slot: &str) -> String {
    let res = book_slot(app, cookie, slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "book slot: {}", res.body);
    assert_eq!(res.body["status"], "pending");
    id_of(&res.body)
}

/// `PATCH /appointments/{id}/{action}` — approve, reject, cancel, or either
/// answer to a counter-proposal (no assertion, no body).
async fn decide(app: &axum::Router, cookie: &str, id: &str, action: &str) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/{action}"),
        Some(cookie),
        None,
    )
    .await
}

/// Counter-propose another window for `id` (no assertion).
async fn propose(
    app: &axum::Router,
    cookie: &str,
    id: &str,
    starts_at: i64,
    ends_at: i64,
) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/reschedule"),
        Some(cookie),
        Some(json!({ "starts_at": starts_at, "ends_at": ends_at })),
    )
    .await
}

#[tokio::test]
async fn appointment_book_and_approve_holds_the_slot() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();

    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": now + HOUR_MS, "ends_at": now + 2 * HOUR_MS,
                "note": "ofis saati" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body.as_array().expect("array body").len(), 1);
    let slot = res.body[0]["id"].as_str().expect("slot id").to_string();
    assert_eq!(res.body[0]["teacher"]["id"], ali_id.as_str());
    assert_eq!(res.body[0]["note"], "ofis saati");
    assert!(res.body[0]["series"].is_null(), "a one-off has no series");

    // Booking lands pending, echoing the slot's own window.
    let res = book_slot(&app, &veli, &slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    assert_eq!(res.body["requester"]["id"], veli_id.as_str());
    assert_eq!(res.body["teacher"]["id"], ali_id.as_str());
    assert_eq!(res.body["starts_at"], now + HOUR_MS);
    assert!(res.body["decided_by"].is_null(), "pending has no decider");
    let booking = id_of(&res.body);

    // The pending request already holds the slot against everyone else.
    let res = book_slot(&app, &ayse, &slot).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    let res = decide(&app, &ali, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["decided_by"]["id"], ali_id.as_str());

    // Both sides read it back from their own list.
    let mine = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(common::items(&mine.body).len(), 1);
    let inbox = send(&app, "GET", "/appointments", Some(&ali), None).await;
    assert_eq!(common::items(&inbox.body)[0]["id"], booking.as_str());
}

#[tokio::test]
async fn rejecting_a_booking_frees_the_slot() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;

    // A bodyless reject records the decider and no reason.
    let booking = book_ok(&app, &veli, &slot).await;
    let res = decide(&app, &ali, &booking, "reject").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "rejected");
    assert_eq!(res.body["decided_by"]["id"], ali_id.as_str());
    assert!(res.body["reject_reason"].is_null(), "no reason was given");

    // A reject carrying a reason records both the decider and the reason, and
    // an over-long reason is refused before the row changes.
    let booking = book_ok(&app, &ayse, &slot).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/reject"),
        Some(&ali),
        Some(json!({ "reason": "x".repeat(2000) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/reject"),
        Some(&ali),
        Some(json!({ "reason": "Bu saatte müsait değilim" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "rejected");
    assert_eq!(res.body["decided_by"]["id"], ali_id.as_str());
    assert_eq!(res.body["reject_reason"], "Bu saatte müsait değilim");

    // Occupancy counts live rows only, so the next person may take it.
    let res = book_slot(&app, &ayse, &slot).await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "a rejected booking must not keep the seat: {}",
        res.body
    );
    // And the rejected one cannot be decided twice.
    let res = decide(&app, &ali, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn approval_refuses_a_teacher_double_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();

    // A teacher can no longer publish two colliding slots, so the collision is
    // reached the only way left: two non-overlapping slots, then a reschedule
    // proposal that drags the second onto the first's just-approved window.
    let first = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let second = publish_slot(&app, &ali, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    let one = book_ok(&app, &veli, &first).await;
    let two = book_ok(&app, &ayse, &second).await;

    assert_eq!(
        decide(&app, &ali, &one, "approve").await.status,
        StatusCode::OK
    );
    // Teacher counter-proposes a window overlapping the just-approved meeting;
    // accepting it is approval at the proposed time, and that collision is what
    // must be refused.
    let res = propose(
        &app,
        &ali,
        &two,
        now + HOUR_MS + 600_000,
        now + 2 * HOUR_MS + 600_000,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = decide(&app, &ayse, &two, "reschedule/accept").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // Refused, not half-applied: it is still pending and still decidable.
    let res = decide(&app, &ali, &two, "reject").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn approval_refuses_a_requester_double_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // One student, two different teachers, overlapping times. Both requests
    // stand (nothing is committed yet); the second approval collides.
    let first = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let second = publish_slot(&app, &mert, now + HOUR_MS + 600_000, now + 3 * HOUR_MS).await;
    let one = book_ok(&app, &veli, &first).await;
    let two = book_ok(&app, &veli, &second).await;

    assert_eq!(
        decide(&app, &ali, &one, "approve").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &mert, &two, "approve").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Booking a fresh overlap once one is committed is refused up front. The
    // slot comes from a third teacher: mert's calendar already holds `second`,
    // which a same-teacher slot at this window would now overlap at publish.
    let ahmet = login_as(&app, &db, "ahmet", "teacher").await;
    let third = publish_slot(&app, &ahmet, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let res = book_slot(&app, &veli, &third).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn touching_windows_never_collide() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Half-open windows: 10:00–11:00 and 11:00–12:00 are back-to-back, not a
    // clash — for the teacher and for the student alike.
    let first = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let second = publish_slot(&app, &ali, now + 2 * HOUR_MS, now + 3 * HOUR_MS).await;
    let one = book_ok(&app, &veli, &first).await;
    let two = book_ok(&app, &veli, &second).await;

    for booking in [&one, &two] {
        let res = decide(&app, &ali, booking, "approve").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["status"], "approved");
    }
}

#[tokio::test]
async fn reschedule_returns_to_pending_until_the_requester_accepts() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    assert_eq!(
        decide(&app, &ali, &booking, "approve").await.status,
        StatusCode::OK
    );

    // A counter-proposal un-commits the approved meeting rather than moving it
    // silently.
    let res = propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    assert!(res.body["decided_by"].is_null(), "approval was withdrawn");
    assert_eq!(res.body["proposed_starts_at"], now + 3 * HOUR_MS);
    assert_eq!(res.body["proposed_ends_at"], now + 4 * HOUR_MS);
    assert_eq!(res.body["proposed_by"]["id"], ali_id.as_str());
    // The effective window already reads as the proposed one.
    assert_eq!(res.body["starts_at"], now + 3 * HOUR_MS);

    // Only the requester may answer it.
    let res = decide(&app, &ayse, &booking, "reschedule/accept").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = decide(&app, &ali, &booking, "reschedule/accept").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    let res = decide(&app, &veli, &booking, "reschedule/accept").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["starts_at"], now + 3 * HOUR_MS);
    assert_eq!(res.body["ends_at"], now + 4 * HOUR_MS);
}

#[tokio::test]
async fn a_started_proposal_is_refused() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // A proposal 30s into the past is inside the publishing grace, so the times
    // themselves validate — but the window is already underway, and a meeting
    // that began is one `cancel` refuses to undo. Refused at the proposal, not
    // left for the requester to agree to.
    let res = propose(&app, &ali, &booking, now - 30_000, now + HOUR_MS).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // Refused, not half-applied: the teacher can still propose a real time.
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(common::items(&res.body)[0]["status"], "pending");
    assert!(
        common::items(&res.body)[0]["proposed_starts_at"].is_null(),
        "a refused proposal leaves nothing behind"
    );

    assert_eq!(
        propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "reschedule/accept").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
}

#[tokio::test]
async fn declining_a_reschedule_cancels_the_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // Nothing proposed yet: there is nothing to decline.
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    assert_eq!(
        propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    // The decliner is recorded as the canceller, with no reason.
    assert_eq!(res.body["cancelled_by"]["id"], veli_id.as_str());
    assert!(res.body["cancel_reason"].is_null());
    // The refused proposal stays readable on the cancelled row.
    assert_eq!(res.body["proposed_starts_at"], now + 3 * HOUR_MS);

    // And the slot is free again.
    let res = book_slot(&app, &ayse, &slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// Declining cancels, so it answers to the cancel deadline. The guard lives in
/// `Appointment::cancel` itself — when it sat in the cancel *handler*, decline
/// went straight past it and called off a meeting already underway.
#[tokio::test]
async fn declining_a_started_reschedule_is_refused() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // A proposal that is still ahead when made, and underway a moment later:
    // the only way an effective window opens with the booking still live.
    let opens_at = Timestamp::now().as_millis() + 700;
    assert_eq!(
        propose(&app, &ali, &booking, opens_at, opens_at + HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // The plain cancel door answers the same, and the booking survives both.
    let res = decide(&app, &veli, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(common::items(&res.body)[0]["status"], "pending");

    // Moved back into the future, declining cancels as it always did.
    assert_eq!(
        propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
}

#[tokio::test]
async fn accepting_a_reschedule_re_runs_the_overlap_guard() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Committed with ali at +5h.
    let taken = publish_slot(&app, &ali, now + 5 * HOUR_MS, now + 6 * HOUR_MS).await;
    let committed = book_ok(&app, &veli, &taken).await;
    assert_eq!(
        decide(&app, &ali, &committed, "approve").await.status,
        StatusCode::OK
    );

    // Mert counter-proposes straight onto that hour; accepting would commit
    // the student twice, so it is refused.
    let slot = publish_slot(&app, &mert, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    assert_eq!(
        propose(&app, &mert, &booking, now + 5 * HOUR_MS, now + 6 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "reschedule/accept").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(
        send(&app, "GET", "/appointments", Some(&veli), None)
            .await
            .body["items"][0]["status"],
        "pending",
        "a refused acceptance leaves the booking pending"
    );
}

/// Only the requester (student/parent) may cancel a booking — not the slot's
/// teacher, not an unrelated student. Deciders reject or reschedule instead.
#[tokio::test]
async fn only_the_requester_cancels() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();

    // The requester's own call, on an approved meeting. A bodyless cancel
    // records the actor and no reason.
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    assert_eq!(
        decide(&app, &ali, &booking, "approve").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    assert_eq!(res.body["cancelled_by"]["id"], veli_id.as_str());
    assert!(res.body["cancel_reason"].is_null(), "no reason was given");

    // A cancel carrying a reason records both the actor and the reason, and an
    // over-long reason is refused before the row changes.
    let booking = book_ok(&app, &veli, &slot).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/cancel"),
        Some(&veli),
        Some(json!({ "reason": "x".repeat(2000) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/cancel"),
        Some(&veli),
        Some(json!({ "reason": "Rahatsızlandım" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    assert_eq!(res.body["cancelled_by"]["id"], veli_id.as_str());
    assert_eq!(res.body["cancel_reason"], "Rahatsızlandım");

    // The slot's teacher may NOT cancel — cancelling is the requester's lever;
    // a decider ends a booking by rejecting (pending) or rescheduling.
    let booking = book_ok(&app, &ayse, &slot).await;
    let res = decide(&app, &ali, &booking, "cancel").await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "owner-teacher: {}",
        res.body
    );
    // An unrelated student may not either.
    let res = decide(&app, &veli, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "outsider: {}", res.body);
    // The requester can, and a settled booking cannot be cancelled twice.
    assert_eq!(
        decide(&app, &ayse, &booking, "cancel").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &ayse, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// A student is not a decider: rejecting a booking is a teacher/manager call,
/// so the requester calling `reject` on their own booking is refused.
#[tokio::test]
async fn student_cannot_reject_a_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    let res = decide(&app, &veli, &booking, "reject").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// A slot-owning teacher may not *cancel* a booking — rejecting (pending) or
/// rescheduling is their lever; cancelling belongs to the requester alone.
#[tokio::test]
async fn teacher_cannot_cancel_a_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Pending booking: owner-teacher cancel is refused (the booking stays live).
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    let res = decide(&app, &ali, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "pending: {}", res.body);

    // Approved booking on a fresh slot: same refusal.
    let slot2 = publish_slot(&app, &ali, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot2).await;
    assert_eq!(
        decide(&app, &ali, &booking, "approve").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &ali, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "approved: {}", res.body);
}

/// A manager who is not the requester also cannot cancel — no oversight override
/// exists; only the requester (student/parent) cancels.
#[tokio::test]
async fn manager_cannot_cancel_a_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mgr = login_as(&app, &db, "mgr", "manager").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    let res = decide(&app, &mgr, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "manager: {}", res.body);
}

#[tokio::test]
async fn a_started_slot_cannot_be_booked() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // A window that opened 30s ago is inside the publishing grace, so the slot
    // is created — but the meeting has begun, and a meeting that began is
    // history, not a plan. Booking it would land a row `cancel` refuses to
    // undo, stuck for good.
    let underway = publish_slot(&app, &ali, now - 30_000, now + HOUR_MS).await;
    let res = book_slot(&app, &veli, &underway).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // A window still ahead is untouched by the guard.
    let upcoming = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let res = book_slot(&app, &veli, &upcoming).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

#[tokio::test]
async fn slot_delete_waits_for_the_booking_to_settle() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    let uri = format!("/appointments/slots/{slot}");
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "someone is waiting on this slot: {}",
        res.body
    );
    assert_eq!(
        decide(&app, &veli, &booking, "cancel").await.status,
        StatusCode::OK
    );
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert_eq!(
        send(&app, "DELETE", &uri, Some(&ali), None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn weekly_slots_share_a_series_and_are_dropped_together() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let starts_at = now + HOUR_MS;

    // `until` is inclusive, so three weeks out is four occurrences.
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": starts_at + HOUR_MS,
                "repeat_weekly": true, "until": starts_at + 3 * WEEK_MS }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let slots = res.body.as_array().expect("array body").clone();
    assert_eq!(slots.len(), 4);
    let series = slots[0]["series"].as_str().expect("series id").to_string();
    for (week, slot) in slots.iter().enumerate() {
        assert_eq!(slot["series"], series.as_str(), "one publish, one series");
        assert_eq!(slot["starts_at"], starts_at + week as i64 * WEEK_MS);
    }

    // `repeat_weekly` without an end, and past the 52-occurrence cap.
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": starts_at + HOUR_MS,
                "repeat_weekly": true }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": starts_at + HOUR_MS,
                "repeat_weekly": true, "until": starts_at + 52 * WEEK_MS }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "53 occurrences: {}",
        res.body
    );

    // An end at `i64::MAX` clears every gate — future, not inverted, 52
    // occurrences — and then overflows on the first weekly shift. Unchecked it
    // wrapped the end below the start, and an inverted window never overlaps
    // anything, so the double-booking guard would have gone blind.
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": i64::MAX,
                "repeat_weekly": true, "until": starts_at + 51 * WEEK_MS }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "a shift that overflows: {}",
        res.body
    );

    // All-or-nothing: one waiting requester anywhere in the series blocks it.
    let booked = slots[1]["id"].as_str().expect("slot id");
    let booking = book_ok(&app, &veli, booked).await;
    let uri = format!("/appointments/slots/series/{series}");
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    assert_eq!(
        decide(&app, &ali, &booking, "reject").await.status,
        StatusCode::OK
    );
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(&app, "GET", "/appointments/slots", Some(&ali), None).await;
    assert_eq!(common::total(&res.body), 0, "the whole series went");
    assert_eq!(
        send(&app, "DELETE", &uri, Some(&ali), None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn only_students_and_parents_book_appointments() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let anne = login_as(&app, &db, "anne", "parent").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;

    // Staff arrange between themselves off this API.
    for (who, label) in [(&mert, "teacher"), (&mudur, "manager"), (&admin, "admin")] {
        let res = book_slot(&app, who, &slot).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{label}: {}", res.body);
    }
    // A parent books for themselves — the parent-teacher conference.
    let res = book_slot(&app, &anne, &slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let second = publish_slot(&app, &ali, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    let res = book_slot(&app, &veli, &second).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

#[tokio::test]
async fn only_the_slots_teacher_decides_its_bookings() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // Another teacher's calendar is not theirs to run.
    for action in ["approve", "reject"] {
        let res = decide(&app, &mert, &booking, action).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{action}: {}", res.body);
    }
    let res = propose(&app, &mert, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/appointments/slots/{slot}"),
        Some(&mert),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    // The requester cannot approve their own request either.
    assert_eq!(
        decide(&app, &veli, &booking, "approve").await.status,
        StatusCode::FORBIDDEN
    );
    // A manager may decide anyone's.
    let res = decide(&app, &mudur, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
}

#[tokio::test]
async fn appointment_times_must_be_sane_and_future() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Past (beyond the 60s grace), empty, and inverted windows.
    for (body, label) in [
        (
            json!({ "starts_at": now - 10 * HOUR_MS, "ends_at": now - 9 * HOUR_MS }),
            "past",
        ),
        (
            json!({ "starts_at": now + HOUR_MS, "ends_at": now + HOUR_MS }),
            "empty",
        ),
        (
            json!({ "starts_at": now + 2 * HOUR_MS, "ends_at": now + HOUR_MS }),
            "inverted",
        ),
    ] {
        let res = publish_slots(&app, &ali, body).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    // A counter-proposal answers to the same rules.
    for (starts_at, ends_at, label) in [
        (now - 10 * HOUR_MS, now - 9 * HOUR_MS, "past"),
        (now + HOUR_MS, now + HOUR_MS, "empty"),
        (now + 2 * HOUR_MS, now + HOUR_MS, "inverted"),
    ] {
        let res = propose(&app, &ali, &booking, starts_at, ends_at).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }
    // A blank reason is refused too — the teacher decides on it.
    let res = send(
        &app,
        "POST",
        "/appointments",
        Some(&veli),
        Some(json!({ "slot": slot, "reason": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

#[tokio::test]
async fn a_demoted_teachers_slots_go_inert() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;

    let res = send(&app, "GET", "/appointments/slots", Some(&veli), None).await;
    assert_eq!(common::items(&res.body).len(), 1, "{}", res.body);

    // The row survives the demotion; the offer does not.
    set_role(&db, "ali", "student").await;
    let res = send(&app, "GET", "/appointments/slots", Some(&veli), None).await;
    assert_eq!(common::total(&res.body), 0, "an inert slot is not offered");
    let res = book_slot(&app, &veli, &slot).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

// --- per-attempt history (retakes preserve prior sittings) ------------------

/// Sit an exam, answer + draw, retake, answer + draw differently — then the
/// teacher's per-attempt history endpoints still surface the seq-1 sitting
/// alongside seq-2: the attempts list is `[1, 2]`, and each seq's answer text
/// and drawing bytes are its own, not the latest sitting's.
#[tokio::test]
async fn attempt_history_preserves_each_prior_sitting() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "hist_t", "teacher").await;
    let student = login(&app, "hist_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "cell quiz", "kind": "quiz", "mode": "open", "max_attempts": 2 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Name an organelle.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let own_image = format!("/exams/{exam}/attempt/answers/{question}/image");

    // Seq 1: answer, draw, finish.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "sit seq 1: {}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "mitochondria" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let (status, _) = post_image(&app, &student, &own_image, "image/png", b"seq1-draw").await;
    assert_eq!(status, StatusCode::CREATED);
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    // Seq 2: retake, answer + draw differently.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["attempt"], 2, "retake is the second sitting");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "chloroplast" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let (status, _) = post_image(&app, &student, &own_image, "image/png", b"seq2-draw").await;
    assert_eq!(status, StatusCode::CREATED);

    // The attempts list carries both sittings, ascending.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1, 2]));

    // Each sitting keeps its own answer text — seq 1 was NOT overwritten.
    let seq1 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts/1/answers"),
        Some(&teacher),
        None,
    )
    .await;
    let seq2 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts/2/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(seq1.body["answers"][0]["text"], "mitochondria");
    assert_eq!(seq2.body["answers"][0]["text"], "chloroplast");
    assert_ne!(
        seq1.body["answers"][0]["text"],
        seq2.body["answers"][0]["text"]
    );

    // Each sitting keeps its own drawing bytes.
    for (seq, want) in [(1, b"seq1-draw".as_slice()), (2, b"seq2-draw".as_slice())] {
        let uri =
            format!("/exams/{exam}/students/{student_id}/attempts/{seq}/answers/{question}/image");
        let (status, _, bytes) =
            common::send_raw(&app, "GET", &uri, Some(&teacher), None, Vec::new()).await;
        assert_eq!(status, StatusCode::OK, "seq {seq} image");
        assert_eq!(bytes, want, "seq {seq} kept its own drawing");
    }
}

/// The grade-of-record follows the latest sitting: grading seq 1, then seq 2
/// with a different mark, leaves the roster read showing seq 2's mark — while
/// the full mark-history endpoint returns both, oldest first.
#[tokio::test]
async fn grade_of_record_is_latest_but_history_keeps_both() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "grd_t", "teacher").await;
    let student = login(&app, "grd_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "drill", "kind": "quiz", "mode": "open", "max_attempts": 2 }),
    )
    .await;

    let results_uri = format!("/exams/{exam}/results");

    // Seq 1: sit, grade 40, finish.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 40, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    // Seq 2: retake, grade 90 — the mark lands on the new sitting.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The roster (latest-per-pair) reports one row, at seq 2's mark.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let rows = common::items(&res.body);
    assert_eq!(rows.len(), 1, "one grade-of-record per student");
    assert_eq!(rows[0]["mark"], 90, "latest sitting is the grade-of-record");

    // The history endpoint keeps both marks, oldest first.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/marks"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let marks: Vec<i64> = res
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["mark"].as_i64().unwrap())
        .collect();
    assert_eq!(marks, vec![40, 90], "both sittings' marks, oldest first");
}

/// The per-attempt history endpoints are teacher-only: an enrolled student
/// asking for another student's history is refused, like every grader read.
#[tokio::test]
async fn attempt_history_endpoints_are_teacher_walled() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "wall_t", "teacher").await;
    let student = login(&app, "wall_s").await;
    let student_id = me_id(&app, &student).await;
    let snooper = login(&app, "wall_o").await;

    let course = create_course(&app, &teacher, "history").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "quiz", "mode": "open" }),
    )
    .await;

    // A non-teacher student is refused on every history route for another student.
    for uri in [
        format!("/exams/{exam}/students/{student_id}/attempts"),
        format!("/exams/{exam}/students/{student_id}/attempts/1/answers"),
        format!("/exams/{exam}/students/{student_id}/marks"),
    ] {
        let res = send(&app, "GET", &uri, Some(&snooper), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri}");
    }
}

// --- student self-review (own-scoped, allow_review + a mark gate it) ---------

/// Sit, answer, teacher grades — but with `allow_review` left at its default
/// (false), the student's own review read is refused: 403, "review not enabled".
#[tokio::test]
async fn self_review_is_forbidden_until_the_teacher_enables_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_off_t", "teacher").await;
    let student = login(&app, "rev_off_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "physics").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "kinematics", "kind": "quiz", "mode": "open" }),
    )
    .await;

    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 55, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// `allow_review` on, but the student has no mark yet — nothing to review, so
/// the gate 404s rather than leaking an empty (or in-progress) sheet.
#[tokio::test]
async fn self_review_is_not_found_until_a_mark_exists() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_ungraded_t", "teacher").await;
    let student = login(&app, "rev_ungraded_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "chemistry").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "moles", "kind": "quiz", "mode": "open", "allow_review": true }),
    )
    .await;

    // Sat, but never graded — no ExamResult row.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// A draft exam is invisible to students even with review on and a mark in hand:
/// the draft check comes first, so every self-review read is a flat 404.
#[tokio::test]
async fn self_review_of_a_draft_exam_is_not_found() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_draft_t", "teacher").await;
    let student = login(&app, "rev_draft_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "geography").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({
            "title": "maps", "kind": "quiz", "mode": "open",
            "allow_review": true, "draft": true,
        }),
    )
    .await;

    for uri in [
        format!("/exams/{exam}/review/attempts"),
        format!("/exams/{exam}/review/attempts/1/answers"),
    ] {
        let res = send(&app, "GET", &uri, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{uri}: {}", res.body);
    }
}

/// The happy path across retakes: review on, both sittings graded, and the
/// student reads their own seqs `[1, 2]` plus each sitting's own answer text —
/// seq 1 was not overwritten by the retake.
#[tokio::test]
async fn self_review_returns_own_seqs_and_per_sitting_answers() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_ok_t", "teacher").await;
    let student = login(&app, "rev_ok_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({
            "title": "cell quiz", "kind": "quiz", "mode": "open",
            "max_attempts": 2, "allow_review": true,
        }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Name an organelle.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let results_uri = format!("/exams/{exam}/results");

    // Seq 1: answer, grade, finish.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "mitochondria" })),
    )
    .await;
    send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 40, "user_id": student_id })),
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    // Seq 2: retake, answer differently, grade.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "chloroplast" })),
    )
    .await;
    send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;

    // Own seqs, ascending.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1, 2]));

    // Each sitting keeps its own answer text.
    let seq1 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts/1/answers"),
        Some(&student),
        None,
    )
    .await;
    let seq2 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts/2/answers"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(seq1.status, StatusCode::OK, "{}", seq1.body);
    assert_eq!(seq1.body["answers"][0]["text"], "mitochondria");
    assert_eq!(seq2.body["answers"][0]["text"], "chloroplast");

    // The answer key: a marked student reads the questions with `correct`
    // exposed (the whole point of review) — the same list the teacher sees.
    let key = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(key.status, StatusCode::OK, "{}", key.body);
    assert_eq!(key.body["items"][0]["id"], question);
    assert!(
        key.body["items"][0]
            .as_object()
            .unwrap()
            .contains_key("correct"),
        "review/questions must expose `correct`: {}",
        key.body
    );
}

/// Own-scoped isolation: two graded students, review on. There is no `{user}`
/// path param, so student A's token resolves to A — A sees A's own seqs and
/// answer text, never B's, even though both are graded on the same exam.
#[tokio::test]
async fn self_review_is_own_scoped_between_students() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_iso_t", "teacher").await;
    let alice = login(&app, "rev_iso_a").await;
    let alice_id = me_id(&app, &alice).await;
    let bob = login(&app, "rev_iso_b").await;
    let bob_id = me_id(&app, &bob).await;

    let course = create_course(&app, &teacher, "history").await;
    let subject = create_subject(&app, &teacher, &course, "rome").await;
    enroll(&app, &teacher, &course, &alice_id).await;
    enroll(&app, &teacher, &course, &bob_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "quiz", "mode": "open", "allow_review": true }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Name a consul.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let results_uri = format!("/exams/{exam}/results");

    for (who, id, text, mark) in [
        (&alice, &alice_id, "cicero", 70),
        (&bob, &bob_id, "cato", 30),
    ] {
        send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt"),
            Some(who),
            None,
        )
        .await;
        send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(who),
            Some(json!({ "question_id": question, "text": text })),
        )
        .await;
        send(
            &app,
            "POST",
            &results_uri,
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": id })),
        )
        .await;
    }

    // Alice's token sees only Alice's data — one seq, her own answer.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1]));
    let sheet = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts/1/answers"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(
        sheet.body["answers"][0]["text"], "cicero",
        "A sees her own answer"
    );
    assert_ne!(sheet.body["answers"][0]["text"], "cato", "never B's answer");
}

// --- question bank -----------------------------------------------------------

#[tokio::test]
async fn bank_question_create_get_list_and_instantiate() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_t", "teacher").await;
    let course = create_course(&app, &teacher, "algebra").await;
    let subject = create_subject(&app, &teacher, &course, "linear").await;

    // Create a template — subject is origin metadata, not held to a course.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "2 + 2?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}, {"id": "c2", "text": "5"}], "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["kind"], "choice");
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);
    assert_eq!(res.body["subject"], subject);
    let bid = id_of(&res.body);

    // School-wide read + list.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["text"], "2 + 2?");
    let res = send(&app, "GET", "/bank-questions", Some(&teacher), None).await;
    assert_eq!(common::items(&res.body).len(), 1);

    // `?subject=` matches, and a foreign id excludes.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?subject={subject}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 1);
    let res = send(
        &app,
        "GET",
        "/bank-questions?subject=nope",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 0);

    // Instantiate into an exam under one of the course's subjects.
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "midterm", "kind": "final", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["subject"], subject);
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);
    assert_eq!(res.body["choices"][1]["text"], "4");

    // The copy is independent — the template still stands.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// The bank list pages in SQL: newest first, every row reachable by offset,
/// `total` counting the matches rather than the page, and `?q=` filtering
/// server-side. Regression for the whole-table-then-slice version, which hid
/// template #101 onward from a client asking for `limit=100`.
#[tokio::test]
async fn bank_question_list_pages_filters_and_names() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_p", "teacher").await;
    let other = login_as(&app, &db, "bank_p2", "teacher").await;
    let course = create_course(&app, &teacher, "algebra").await;
    let subject = create_subject(&app, &teacher, &course, "linear").await;
    let other_subject = create_subject(&app, &teacher, &course, "quadratic").await;

    // 120 templates — more than the clients' `limit=100`.
    for i in 0..120 {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(
                json!({ "subject_id": subject, "text": format!("template {i:03}"),
                         "kind": "text", "points": 1 }),
            ),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    // A foreign owner + subject, to prove the filters actually narrow.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&other),
        Some(
            json!({ "subject_id": other_subject, "text": "someone else's",
                     "kind": "text", "points": 1 }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // Published, so it shows up in the first teacher's paging assertions below;
    // a private one would (correctly) be invisible to them.
    let foreign = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{foreign}"),
        Some(&other),
        Some(json!({ "visibility": "school" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Page 1: newest first, full window, honest total.
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 100);
    assert_eq!(res.body["total"], 121);
    assert_eq!(common::items(&res.body)[0]["text"], "someone else's");
    assert_eq!(common::items(&res.body)[1]["text"], "template 119");
    // The names are joined on, not left for the client to look up per row.
    assert_eq!(common::items(&res.body)[1]["subject_name"], "linear");
    assert_eq!(common::items(&res.body)[1]["owner_name"], "bank_p");

    // Item #101 onward is reachable — the bug was that it never was.
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100&offset=100",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 21);
    assert_eq!(res.body["total"], 121);
    assert_eq!(common::items(&res.body)[0]["text"], "template 020");
    assert_eq!(common::items(&res.body)[20]["text"], "template 000");

    // `?q=` filters server-side: "template 005" sits on page 2 unfiltered, but
    // leads page 1 once filtered — and matches case-insensitively.
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100&q=TEMPLATE%20005",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(common::items(&res.body)[0]["text"], "template 005");
    // A blank `q` is no filter at all.
    let res = send(&app, "GET", "/bank-questions?q=", Some(&teacher), None).await;
    assert_eq!(res.body["total"], 121);

    // `owner=me` + `subject=` combine, and still page.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner=me&subject={subject}&limit=100&offset=100"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 120);
    assert_eq!(common::items(&res.body).len(), 20);
    assert_eq!(common::items(&res.body)[0]["text"], "template 019");
    // The other teacher's `me` is their own single template.
    let res = send(&app, "GET", "/bank-questions?owner=me", Some(&other), None).await;
    assert_eq!(res.body["total"], 1);
    assert_eq!(common::items(&res.body)[0]["owner_name"], "bank_p2");
    // Owner and subject that never co-occur → an empty page, total 0.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner=me&subject={other_subject}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0);
    assert!(common::items(&res.body).is_empty());
}

/// `?visibility=` is a real SQL filter (so the client never has to fetch the
/// bank unpaged and window it in memory — the very bug the SQL paging fixed),
/// and it narrows the caller's view without ever widening it.
#[tokio::test]
async fn bank_question_list_filters_by_visibility() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_v", "teacher").await;
    let other = login_as(&app, &db, "bank_v2", "teacher").await;
    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    let other_subject = create_subject(&app, &teacher, &course, "genes").await;

    // Mine: 3 drafts on `subject`, 2 published on `other_subject`.
    let mut published = Vec::new();
    for i in 0..5 {
        let on = if i < 3 { &subject } else { &other_subject };
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": on, "text": format!("mine {i}"),
                         "kind": "text", "points": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        if i >= 3 {
            published.push(id_of(&res.body));
        }
    }
    for bid in &published {
        let res = send(
            &app,
            "PATCH",
            &format!("/bank-questions/{bid}"),
            Some(&teacher),
            Some(json!({ "visibility": "school" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    // The other teacher keeps one private template — nobody else may ever see
    // it, whatever they pass in `?visibility=`.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&other),
        Some(json!({ "subject_id": subject, "text": "their secret",
                     "kind": "text", "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // `private` = my drafts; `total` honours the filter, not the whole bank.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 3, "{}", res.body);
    assert_eq!(common::items(&res.body).len(), 3);
    assert_eq!(common::items(&res.body)[0]["text"], "mine 2");
    // `school` = the published shelf — and NOT the other teacher's private row.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=school",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 2, "{}", res.body);
    let texts: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|item| item["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["mine 4", "mine 3"]);
    assert!(!texts.contains(&"their secret"));
    // The stranger's `private` view is empty: the filter can't widen the gate.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private",
        Some(&teacher),
        None,
    )
    .await;
    assert!(
        !common::items(&res.body)
            .iter()
            .any(|item| item["text"] == "their secret")
    );
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private",
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1, "{}", res.body);
    assert_eq!(common::items(&res.body)[0]["text"], "their secret");

    // Composes with `q`, `subject`, `owner`, and with paging.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?visibility=private&subject={subject}&owner=me&q=MINE%201"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1, "{}", res.body);
    assert_eq!(common::items(&res.body)[0]["text"], "mine 1");
    // Right owner and subject, wrong shelf → nothing.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?visibility=school&subject={subject}&owner=me"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0, "{}", res.body);
    assert!(common::items(&res.body).is_empty());
    // Paging windows the filtered set, and `total` stays the filtered total.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private&limit=2&offset=2",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 3, "{}", res.body);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(common::items(&res.body)[0]["text"], "mine 0");

    // An unknown shelf is a 400, worded like the other param errors.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=public",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(
        res.body["error"]
            .as_str()
            .unwrap()
            .contains("must be private or school"),
        "{}",
        res.body
    );
}

/// The app is EN/TR, so `?q=` must fold Turkish casing: `İ` lowercases to
/// `i` + U+0307 in both Rust and SurrealQL, which used to make `istanbul` and
/// `İSTANBUL` two disjoint searches — a teacher typing lowercase got an empty
/// bank.
#[tokio::test]
async fn bank_question_search_folds_turkish_casing() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_tr", "teacher").await;
    let course = create_course(&app, &teacher, "cografya").await;
    let subject = create_subject(&app, &teacher, &course, "iller").await;
    for text in ["İSTANBUL kaç ilçedir?", "istanbul boğazı nerededir?"] {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": subject, "text": text,
                         "kind": "text", "points": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    // Lowercase needle finds the uppercase row, and every casing finds both.
    for needle in ["istanbul", "%C4%B0STANBUL", "%C4%B1stanbul", "ISTANBUL"] {
        let res = send(
            &app,
            "GET",
            &format!("/bank-questions?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.body["total"], 2, "needle {needle} missed: {}", res.body);
    }
    // Diacritics fold too, in both directions.
    for needle in ["ilce", "il%C3%A7e", "bogaz", "bo%C4%9Faz"] {
        let res = send(
            &app,
            "GET",
            &format!("/bank-questions?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.body["total"], 1, "needle {needle} missed: {}", res.body);
    }
    // Folding widens the match, it does not match everything.
    let res = send(
        &app,
        "GET",
        "/bank-questions?q=ankara",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0);
}

#[tokio::test]
async fn bank_question_owner_gate_cross_course_and_freeze() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "bank_o", "teacher").await;
    let other = login_as(&app, &db, "bank_x", "teacher").await;
    let admin = login_as(&app, &db, "bank_a", "admin").await;

    let course = create_course(&app, &owner, "geo").await;
    let subject = create_subject(&app, &owner, &course, "angles").await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);
    // Published, so this test is about the *owner* gate alone — the visibility
    // gate (404 while private) has its own test below.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&owner),
        Some(json!({ "visibility": "school" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A non-owner teacher reads it, but may not edit or delete.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&other),
        Some(json!({ "points": 7 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/bank-questions/{bid}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // Admin bypasses ownership.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&admin),
        Some(json!({ "points": 7 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["points"], 7);

    // Instantiate with a subject from ANOTHER course is a 400.
    let course2 = create_course(&app, &owner, "chem").await;
    let subject2 = create_subject(&app, &owner, &course2, "bonds").await;
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &owner,
        &course,
        json!({ "title": "t", "kind": "final", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&owner),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Once a student sits the exam, instantiate freezes (409).
    let student = login(&app, "sinem").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &owner, &course, &student_id).await;
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
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&owner),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// A bank template carries `correct` — the answer key — so it is `private`
/// until its owner publishes it. Regression for the one-click answer-key
/// broadcast: saving a live exam's question to the bank used to hand the key
/// (and the pictures) to every teacher in the school, permanently and silently.
///
/// Every read path is checked from a second teacher's seat, and each one must
/// answer **404, not 403** — a 403 confirms the template exists.
#[tokio::test]
async fn bank_private_template_is_invisible_until_published() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "bvis_o", "teacher").await;
    let other = login_as(&app, &db, "bvis_x", "teacher").await;
    let course = create_course(&app, &owner, "bio").await;
    let subject = create_subject(&app, &owner, &course, "cells").await;

    // Two templates; only the second is ever published.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "Which organelle?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "nucleus"}, {"id": "c1", "text": "ribosome"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // New templates are born private — nothing has to opt in to be safe.
    assert_eq!(res.body["visibility"], "private");
    let secret = id_of(&res.body);
    let opt = choice_of(&res.body, 0);
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "shared later", "kind": "text", "points": 1 })),
    )
    .await;
    let shared = id_of(&res.body);

    let illus = b"private-illustration".as_slice();
    let pic = b"private-choice-pic".as_slice();
    let (status, _) = post_image(
        &app,
        &owner,
        &format!("/bank-questions/{secret}/image"),
        "image/png",
        illus,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &owner,
        &format!("/bank-questions/{secret}/choices/{opt}/image"),
        "image/webp",
        pic,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The other teacher's own exam, to attempt an instantiate from.
    let course2 = create_course(&app, &other, "bio2").await;
    let subject2 = create_subject(&app, &other, &course2, "cells2").await;
    let exam2 = create_exam(&app, &other, &course2, "quiz", "final").await;
    let owner_id = me_id(&app, &owner).await;

    // --- the stranger sees nothing, and is told nothing ---
    let res = send(&app, "GET", "/bank-questions", Some(&other), None).await;
    assert!(common::items(&res.body).is_empty());
    assert_eq!(
        res.body["total"], 0,
        "total must count only visible templates"
    );
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{secret}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/image"),
        Some(&other),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/choices/{opt}/image"),
        Some(&other),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{secret}"),
        Some(&other),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // Not even by asking for the owner's templates by name.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner={owner_id}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0);

    // --- the owner still has full use of their own template ---
    let res = send(&app, "GET", "/bank-questions", Some(&owner), None).await;
    assert_eq!(res.body["total"], 2);
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{secret}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["correct"], res.body["choices"][0]["id"]);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/image"),
        Some(&owner),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, illus));
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/choices/{opt}/image"),
        Some(&owner),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, pic));
    let exam = create_exam(&app, &owner, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{secret}"),
        Some(&owner),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // --- publishing one template opens exactly that one ---
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{shared}"),
        Some(&owner),
        Some(json!({ "visibility": "school" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["visibility"], "school");
    let res = send(&app, "GET", "/bank-questions", Some(&other), None).await;
    assert_eq!(res.body["total"], 1, "the private one must still be hidden");
    assert_eq!(common::items(&res.body)[0]["id"], shared);
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{shared}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{shared}"),
        Some(&other),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // The still-private one stays shut on every path.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{secret}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{secret}"),
        Some(&other),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // Unpublishing closes it again.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{shared}"),
        Some(&owner),
        Some(json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{shared}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    // A bogus value is a 400, not a silent publish.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{shared}"),
        Some(&owner),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// Admins see private templates: they already read every exam's `correct` via
/// course management, and they may already delete any template.
#[tokio::test]
async fn bank_private_template_is_visible_to_admins() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "badm_o", "teacher").await;
    let admin = login_as(&app, &db, "badm_a", "admin").await;
    let course = create_course(&app, &owner, "hist").await;
    let subject = create_subject(&app, &owner, &course, "ottoman").await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "when?", "kind": "text", "points": 1 })),
    )
    .await;
    let bid = id_of(&res.body);

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/bank-questions", Some(&admin), None).await;
    assert_eq!(res.body["total"], 1);
}

#[tokio::test]
async fn exam_question_saves_to_bank() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "s2b_t", "teacher").await;
    let course = create_course(&app, &teacher, "phys").await;
    let subject = create_subject(&app, &teacher, &course, "kinematics").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "v=?", "kind": "choice", "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["text"], "v=?");
    assert_eq!(res.body["correct"], res.body["choices"][0]["id"]);
    assert_eq!(res.body["subject"], subject);
    assert_eq!(res.body["owner"], me_id(&app, &teacher).await);
    // The one-click broadcast that was: the saved key stays the teacher's until
    // they publish it.
    assert_eq!(res.body["visibility"], "private");
    let bid = id_of(&res.body);

    // It now lives in their bank.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// Instantiating a bank template copies its image blobs to fresh files under the
/// new exam question, byte-for-byte, and leaves the source template's images
/// untouched.
#[tokio::test]
async fn bank_instantiate_copies_images() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bimg_t", "teacher").await;
    let course = create_course(&app, &teacher, "geo1").await;
    let subject = create_subject(&app, &teacher, &course, "maps1").await;

    // A choice template with an illustration and one option picture.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "Which city?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "Ankara"}, {"id": "c1", "text": "İzmir"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);

    let illus = b"bank-illustration".as_slice();
    let choice = b"bank-choice-pic".as_slice();
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        illus,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        "image/webp",
        choice,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let src_bank_keys = bank_image_blob_keys(&db).await;
    assert_eq!(src_bank_keys.len(), 2);

    // Instantiate into an exam.
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid = id_of(&res.body);

    // Both images retrievable on the new exam question, bytes equal to source.
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/choices/{opt}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, choice);

    // Copies are fresh files — new keys, none shared with the bank.
    let exam_keys = image_blob_keys(&db).await;
    assert_eq!(exam_keys.len(), 2);
    for k in &exam_keys {
        assert!(
            !src_bank_keys.contains(k),
            "copied blob reused a source file"
        );
    }

    // Source bank images untouched — still 200 with the same bytes and keys.
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, choice);
    assert_eq!(
        bank_image_blob_keys(&db).await,
        src_bank_keys,
        "source keys changed"
    );
}

/// Saving an exam question to the bank copies its image blobs to fresh files
/// under the new template, byte-for-byte, source exam images untouched.
#[tokio::test]
async fn bank_save_copies_images() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "simg_t", "teacher").await;
    let course = create_course(&app, &teacher, "phys1").await;
    let subject = create_subject(&app, &teacher, &course, "kin1").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let qid_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "v=?", "kind": "choice", "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let qid = id_of(&qid_body);
    let opt = choice_of(&qid_body, 0);

    let illus = b"exam-illustration".as_slice();
    let choice = b"exam-choice-pic".as_slice();
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{qid}/image"),
        "image/png",
        illus,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{qid}/choices/{opt}/image"),
        "image/jpeg",
        choice,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let src_exam_keys = image_blob_keys(&db).await;
    assert_eq!(src_exam_keys.len(), 2);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);

    // The new bank question carries both images, bytes equal.
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, choice);

    // Fresh keys; source exam images untouched.
    let bank_keys = bank_image_blob_keys(&db).await;
    assert_eq!(bank_keys.len(), 2);
    for k in &bank_keys {
        assert!(
            !src_exam_keys.contains(k),
            "copied blob reused a source file"
        );
    }
    assert_eq!(
        image_blob_keys(&db).await,
        src_exam_keys,
        "source keys changed"
    );
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
}

/// An image survives a full round trip: exam question -> bank -> a second exam,
/// with the bytes intact at the far end.
#[tokio::test]
async fn bank_round_trip_preserves_image_bytes() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rt_t", "teacher").await;
    let course = create_course(&app, &teacher, "hist1").await;
    let subject = create_subject(&app, &teacher, &course, "eras1").await;
    let exam1 = create_exam(&app, &teacher, &course, "e1", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam1,
        &subject,
        json!({ "text": "When?", "kind": "text", "points": 10 }),
    )
    .await;
    let pic = b"round-trip-illustration".as_slice();
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam1}/questions/{qid}/image"),
        "image/png",
        pic,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Hop one: exam -> bank.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam1}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);

    // Hop two: bank -> a second exam.
    let exam2 = create_exam(&app, &teacher, &course, "e2", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid2 = id_of(&res.body);

    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam2}/questions/{qid2}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, pic, "bytes drifted across the round trip");
}

/// Gap-1 guard: a missing source blob mid-copy is all-or-nothing — the
/// instantiate 500s and leaves NO partial destination (no exam question row, no
/// exam image rows), while the source template survives intact.
#[tokio::test]
async fn bank_instantiate_rolls_back_on_missing_source_blob() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rb_t", "teacher").await;
    let course = create_course(&app, &teacher, "geo2").await;
    let subject = create_subject(&app, &teacher, &course, "maps2").await;

    // Two images so the copy loop has multiple steps.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" })),
    )
    .await;
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"illus",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        "image/png",
        b"opt",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Corrupt the source on disk: delete one blob so the copy loop hits an
    // unreadable source mid-flight.
    let src_keys = bank_image_blob_keys(&db).await;
    assert_eq!(src_keys.len(), 2);
    tokio::fs::remove_file(common::files_dir().join(&src_keys[0]))
        .await
        .unwrap();

    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        res.body
    );

    // No partial destination: no question row, no exam image rows (isolated
    // per-test DB is the race-safe source of truth; the shared parallel blob
    // dir rules out an exact on-disk diff).
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert!(
        common::items(&res.body).is_empty(),
        "orphan question survived rollback"
    );
    assert!(
        image_blob_keys(&db).await.is_empty(),
        "orphan exam image rows survived rollback"
    );

    // Source intact: the template still reads, and its surviving blob is still
    // on disk.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        bank_image_blob_keys(&db).await,
        src_keys,
        "source bank rows changed"
    );
    assert!(
        common::files_dir().join(&src_keys[1]).exists(),
        "surviving source blob vanished"
    );
}

/// A bank template's subject is origin metadata, but it must still exist:
/// create and update both 400 on an unknown subject id.
#[tokio::test]
async fn bank_rejects_unknown_subject() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bsub_t", "teacher").await;
    let course = create_course(&app, &teacher, "bsub_c").await;
    let subject = create_subject(&app, &teacher, &course, "bsub_s").await;

    // Create with a bogus subject id — 400, not a stored row.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": "nope", "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Create a real one, then PATCH its subject to a bogus id — also 400.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// Deleting a subject clears it off the bank templates that carried it as
/// origin metadata, instead of being refused. Blocking would have been a dead
/// end (only a template's owner may re-tag it, so a manager could not clear
/// their own 409) and an existence oracle for other teachers' private
/// templates — hence the cascade.
#[tokio::test]
async fn subject_delete_clears_the_bank_templates_subject() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bdel_t", "teacher").await;
    let manager = login_as(&app, &db, "bdel_m", "manager").await;
    let course = create_course(&app, &teacher, "bdel_c").await;
    let subject = create_subject(&app, &teacher, &course, "bdel_s").await;
    let other = create_subject(&app, &teacher, &course, "bdel_s2").await;

    let mut ids = Vec::new();
    for subject_id in [&subject, &other] {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": subject_id, "text": "q", "kind": "text", "points": 5 })),
        )
        .await;
        ids.push(id_of(&res.body));
    }
    let (bid, kept) = (&ids[0], &ids[1]);

    // The manager owns none of these templates, yet the delete goes through.
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{subject}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // The template survives with a cleared origin; the other one is untouched.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], serde_json::Value::Null);
    assert_eq!(res.body["text"], "q");
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{kept}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["subject"], json!(other));

    // The list join renders the cleared one with an empty subject name.
    let res = send(&app, "GET", "/bank-questions", Some(&teacher), None).await;
    let cleared = common::items(&res.body)
        .iter()
        .find(|q| q["id"] == *bid)
        .expect("template in list")
        .clone();
    assert_eq!(cleared["subject"], serde_json::Value::Null);
    assert_eq!(cleared["subject_name"], "");

    // A cleared template still edits: PATCH without `subject_id` keeps it null.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        Some(json!({ "text": "still editable" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], serde_json::Value::Null);
    assert_eq!(res.body["text"], "still editable");
}

/// `?owner=me` (and an explicit owner id) narrows the bank to one owner's
/// templates — on top of the visibility gate, never around it.
#[tokio::test]
async fn bank_owner_filter() {
    let (app, db) = app_and_db().await;
    let alice = login_as(&app, &db, "bown_a", "teacher").await;
    let bob = login_as(&app, &db, "bown_b", "teacher").await;
    let course = create_course(&app, &alice, "bown_c").await;
    let subject = create_subject(&app, &alice, &course, "bown_s").await;

    for who in [&alice, &bob] {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(who),
            Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
        )
        .await;
        // Published, so this test is about the owner filter, not visibility.
        let bid = id_of(&res.body);
        let res = send(
            &app,
            "PATCH",
            &format!("/bank-questions/{bid}"),
            Some(who),
            Some(json!({ "visibility": "school" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    // Whole bank has both.
    let res = send(&app, "GET", "/bank-questions", Some(&alice), None).await;
    assert_eq!(common::items(&res.body).len(), 2);

    // `me` resolves to the caller.
    let res = send(&app, "GET", "/bank-questions?owner=me", Some(&alice), None).await;
    let items = common::items(&res.body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["owner"], alice_id);

    // An explicit id filters to that owner.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner={bob_id}"),
        Some(&alice),
        None,
    )
    .await;
    let items = common::items(&res.body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["owner"], bob_id);
}

/// A template's response carries its image metadata (illustration + per-choice
/// slots) once images are uploaded — the same shape the exam side returns.
#[tokio::test]
async fn bank_response_carries_image_metas() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bmeta_t", "teacher").await;
    let course = create_course(&app, &teacher, "bmeta_c").await;
    let subject = create_subject(&app, &teacher, &course, "bmeta_s").await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" })),
    )
    .await;
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);

    // No images yet: metas are null / all-empty.
    assert!(res.body["image"].is_null());
    assert_eq!(res.body["choice_images"], json!([null, null]));

    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"illus",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        "image/jpeg",
        b"opt",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["image"]["content_type"], "image/png");
    assert_eq!(res.body["image"]["size"], 5);
    assert_eq!(res.body["choice_images"][0]["content_type"], "image/jpeg");
    assert!(res.body["choice_images"][1].is_null());

    // The list carries them too (batched, not per-row).
    let res = send(
        &app,
        "GET",
        "/bank-questions?owner=me",
        Some(&teacher),
        None,
    )
    .await;
    let items = common::items(&res.body);
    assert_eq!(items[0]["image"]["content_type"], "image/png");
    assert_eq!(items[0]["choice_images"][0]["content_type"], "image/jpeg");
}

/// Provenance rides both directions, one column per direction: instantiate
/// records `from_bank` on the new exam question (and leaves `banked_as` null —
/// coming *from* the bank is not being saved *to* it); save-to-bank records
/// `source_exam` on the new template and `banked_as` on the question, without
/// touching `from_bank`.
#[tokio::test]
async fn bank_provenance_both_directions() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bprov_t", "teacher").await;
    let course = create_course(&app, &teacher, "bprov_c").await;
    let subject = create_subject(&app, &teacher, &course, "bprov_s").await;

    // Bank -> exam: from_bank is the template id, banked_as stays null.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["from_bank"], bid);
    // The lie the split fixes: an inserted question must not read as banked.
    assert!(res.body["banked_as"].is_null(), "{}", res.body);
    let inserted = id_of(&res.body);

    // Saving that inserted question sets banked_as and leaves from_bank alone.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{inserted}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let template = id_of(&res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    let question = common::items(&res.body)
        .iter()
        .find(|q| q["id"] == inserted)
        .expect("question in list")
        .clone();
    assert_eq!(question["from_bank"], bid, "{question}");
    assert_eq!(question["banked_as"], template, "{question}");

    // A directly authored exam question has neither link.
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;

    // Exam -> bank: source_exam is the origin exam id.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["source_exam"], exam);

    // A directly authored template has no source_exam.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert!(res.body["source_exam"].is_null());
}

/// Save-to-bank also links the *question* back at the template it produced, so
/// a client can tell it was already banked. A repeat save is allowed and
/// repoints the link at the newer template.
#[tokio::test]
async fn to_bank_links_the_question_at_the_new_template() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bback_t", "teacher").await;
    let course = create_course(&app, &teacher, "bback_c").await;
    let subject = create_subject(&app, &teacher, &course, "bback_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;

    // Fresh question: no link yet.
    let banked_as_of = |body: &serde_json::Value| -> serde_json::Value {
        common::items(body)
            .iter()
            .find(|q| q["id"] == qid)
            .expect("question in list")["banked_as"]
            .clone()
    };
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert!(banked_as_of(&res.body).is_null());

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let first = id_of(&res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(banked_as_of(&res.body), json!(first));

    // Second save: still 201, a distinct template, and the link follows it.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let second = id_of(&res.body);
    assert_ne!(first, second);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(banked_as_of(&res.body), json!(second));
}

/// The back-link is a field-scoped write. `question_to_bank` reads the question,
/// then awaits a bank insert and the whole blob-copy loop before linking, holding
/// no lock over `exam_question` — so a `PATCH` can land in that window. A
/// whole-row save from the stale struct would silently revert it. Driven at the
/// domain level: HTTP offers no way to interleave inside the handler.
#[tokio::test]
async fn linking_the_banked_template_does_not_clobber_a_concurrent_edit() {
    use hezarfen_backend::domain::bank_question::BankQuestionId;
    use hezarfen_backend::domain::exam_question::{ExamQuestion, ExamQuestionId};

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bclob_t", "teacher").await;
    let course = create_course(&app, &teacher, "bclob_c").await;
    let subject = create_subject(&app, &teacher, &course, "bclob_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "original", "kind": "text", "points": 5 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);

    // The handler's read, then someone else's edit lands mid-window.
    let stale = ExamQuestion::read(&ExamQuestionId::from_key(&qid), &db)
        .await
        .unwrap()
        .expect("question exists");
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{qid}"),
        Some(&teacher),
        Some(json!({ "text": "edited by someone else", "points": 9 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    stale
        .link_banked_as(BankQuestionId::from_key(&bid), &db)
        .await
        .expect("link written");

    // The edit survives and the link is set.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    let question = common::items(&res.body)
        .iter()
        .find(|q| q["id"] == qid)
        .expect("question in list")
        .clone();
    assert_eq!(question["text"], "edited by someone else");
    assert_eq!(question["points"], 9);
    assert_eq!(question["banked_as"], bid);
}

/// A bank PATCH writes only the fields it was given: the handler reads the
/// template, then awaits an ownership check, a subject lookup and (on a choice
/// replace) an image sweep, so another write can land in that window — here the
/// exam-delete cascade clearing `source_exam`, a field the PATCH never sets.
/// A whole-row save from the stale struct would resurrect the dead exam link.
#[tokio::test]
async fn a_bank_patch_does_not_clobber_a_concurrent_write() {
    use hezarfen_backend::domain::bank_question::{BankQuestion, BankQuestionId, BankVisibility};
    use hezarfen_backend::domain::exam_question::{
        QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
    };

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bpatch_t", "teacher").await;
    let course = create_course(&app, &teacher, "bpatch_c").await;
    let subject = create_subject(&app, &teacher, &course, "bpatch_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "original", "kind": "text", "points": 5 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    assert_eq!(res.body["source_exam"], json!(exam));

    // The handler's read, then the exam delete lands mid-window and clears the
    // template's `source_exam`.
    let stale = BankQuestion::read(&BankQuestionId::from_key(&bid), &db)
        .await
        .unwrap()
        .expect("template exists");
    assert!(stale.get_source_exam().is_some());
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // The stale struct writes its own fields only.
    let updated = stale
        .update(
            None,
            QuestionText::try_new("edited").unwrap(),
            QuestionPoints::try_new(7).unwrap(),
            QuestionSpec::try_new(QuestionKind::try_new("text").unwrap(), None, None, &[]).unwrap(),
            BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap(),
            &db,
        )
        .await
        .expect("update written");
    assert_eq!(updated.get_text().as_str(), "edited");
    assert!(
        updated.get_source_exam().is_none(),
        "the dead exam link came back"
    );

    // The edit landed and the cleared provenance stayed cleared.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["source_exam"], serde_json::Value::Null);
    assert_eq!(res.body["subject"], serde_json::Value::Null);
    assert_eq!(res.body["text"], "edited");
    assert_eq!(res.body["points"], 7);
    assert_eq!(res.body["visibility"], "school");
}

/// Deleting a bank template clears *both* provenance links that can point at
/// it — `from_bank` on the questions instantiated from it and `banked_as` on
/// the question it was saved out of — instead of leaving either pointing at a
/// dead row.
#[tokio::test]
async fn deleting_a_template_clears_both_question_links() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bdang_t", "teacher").await;
    let course = create_course(&app, &teacher, "bdang_c").await;
    let subject = create_subject(&app, &teacher, &course, "bdang_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;

    // Question A: authored here, then saved to the bank -> banked_as = saved.
    let authored = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{authored}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let saved = id_of(&res.body);

    // Question B: instantiated from a separate template -> from_bank = origin.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "t", "kind": "text", "points": 5 })),
    )
    .await;
    let origin = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{origin}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let inserted = id_of(&res.body);

    let link_of = |body: &serde_json::Value, qid: &str, field: &str| -> serde_json::Value {
        common::items(body)
            .iter()
            .find(|q| q["id"] == qid)
            .expect("question in list")[field]
            .clone()
    };
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(link_of(&res.body, &authored, "banked_as"), json!(saved));
    assert_eq!(link_of(&res.body, &inserted, "from_bank"), json!(origin));

    for bid in [&saved, &origin] {
        let res = send(
            &app,
            "DELETE",
            &format!("/bank-questions/{bid}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    }

    // Both questions survive their templates with no dangling link.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(
        link_of(&res.body, &authored, "banked_as").is_null(),
        "{}",
        res.body
    );
    assert!(
        link_of(&res.body, &inserted, "from_bank").is_null(),
        "{}",
        res.body
    );
}

/// The mirror: deleting the origin exam clears `source_exam` on the templates
/// saved out of it — the template is a reusable library row and outlives the
/// exam, but the bank page must not read a link to a dead exam.
#[tokio::test]
async fn deleting_an_exam_clears_the_templates_source_exam() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "edang_t", "teacher").await;
    let course = create_course(&app, &teacher, "edang_c").await;
    let subject = create_subject(&app, &teacher, &course, "edang_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    assert_eq!(res.body["source_exam"], exam);

    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["source_exam"].is_null(), "{}", res.body);
}

/// `used_count` on the bank list: how many exam questions were copied out of
/// each template. It is the divergence surface made visible — every one of
/// those copies is detached, so an edit here never reaches them. Joined once
/// for the whole page (one grouped query), list-only exactly like
/// `subject_name`/`owner_name`.
#[tokio::test]
async fn bank_list_counts_the_copies_made_from_each_template() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "buse_t", "teacher").await;
    let course = create_course(&app, &teacher, "buse_c").await;
    let subject = create_subject(&app, &teacher, &course, "buse_s").await;

    let template = |text: &'static str| {
        send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": subject, "text": text, "kind": "text", "points": 4 })),
        )
    };
    let used = id_of(&template("reused").await.body);
    let unused = id_of(&template("never used").await.body);

    // Two exams, three copies: twice out of `used` in one exam, once in the other.
    let mut exams = Vec::new();
    for title in ["one", "two"] {
        exams.push(create_exam(&app, &teacher, &course, title, "final").await);
    }
    for (exam, times) in [(&exams[0], 2), (&exams[1], 1)] {
        for _ in 0..times {
            let res = send(
                &app,
                "POST",
                &format!("/exams/{exam}/questions/from-bank/{used}"),
                Some(&teacher),
                Some(json!({ "subject_id": subject })),
            )
            .await;
            assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        }
    }
    // A hand-authored question in the same exam has no template and must not
    // be tallied against one.
    create_question(
        &app,
        &teacher,
        &exams[0],
        &subject,
        json!({ "text": "mine alone", "kind": "text", "points": 1 }),
    )
    .await;

    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let counts: std::collections::HashMap<String, i64> = common::items(&res.body)
        .iter()
        .map(|item| (id_of(item), item["used_count"].as_i64().unwrap()))
        .collect();
    assert_eq!(counts.get(&used).copied(), Some(3), "{}", res.body);
    assert_eq!(counts.get(&unused).copied(), Some(0), "{}", res.body);

    // Deleting a copy takes it back off the tally (its `from_bank` goes with it).
    let res = send(
        &app,
        "GET",
        &format!("/exams/{}/questions", exams[1]),
        Some(&teacher),
        None,
    )
    .await;
    let qid = id_of(&common::items(&res.body)[0]);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{}/questions/{qid}", exams[1]),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100",
        Some(&teacher),
        None,
    )
    .await;
    let after = common::items(&res.body)
        .iter()
        .find(|item| id_of(item) == used)
        .cloned()
        .unwrap();
    assert_eq!(after["used_count"], 2, "{}", res.body);

    // List-only, like the other joined fields: the single-template GET says 0.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{used}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["used_count"], 0, "{}", res.body);
}

/// The escape hatch out of the divergence trap: re-copy a template's *current*
/// content over the exam question that was instantiated from it.
///
/// Choice ids come from the template — the same funnel the original insert
/// used — so pictures land back on the right options and a stale option's
/// picture goes with it. A student's recorded `selected` can never be stranded
/// by that: the route dies on `ensure_questions_editable` (409) the moment any
/// attempt exists, and answers only exist under attempts.
#[tokio::test]
async fn refreshing_a_question_recopies_its_template() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bref_t", "teacher").await;
    let manager = login_as(&app, &db, "bref_m", "manager").await;
    let course = create_course(&app, &teacher, "bref_c").await;
    let subject = create_subject(&app, &teacher, &course, "bref_s").await;

    // A template with an illustration and a picture on its first option.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "capitol of France?", "kind": "choice",
                     "points": 5, "choices": [{"id": "a", "text": "Paris"}, {"id": "b", "text": "Lyon"}],
                     "correct": "a" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    let keep_choice = choice_of(&res.body, 0);
    let drop_choice = choice_of(&res.body, 1);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"old-illustration",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{drop_choice}/image"),
        "image/png",
        b"lyon-picture",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid = id_of(&res.body);

    // The exam copy drifts (a local edit), and the template is fixed and
    // reshaped: the typo goes, "Lyon" is replaced by a new option (a new id),
    // points change, and the illustration is replaced.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{qid}"),
        Some(&teacher),
        Some(json!({ "text": "local edit that will be lost", "points": 99 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        Some(json!({ "text": "capital of France?", "points": 7,
                     "choices": [{"id": keep_choice, "text": "Paris"}, {"id": "new", "text": "Marseille"}],
                     "correct": keep_choice })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let fresh_choice = choice_of(&res.body, 1);
    assert_ne!(fresh_choice, drop_choice, "the replaced option is a new id");
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"new-illustration",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A template the caller cannot see is a 404 on this route too — never a
    // 403, which would confirm it exists. (A manager may manage the course but
    // is not the template's owner, and the template is private.)
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/refresh-from-bank"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);

    // The refresh itself: content, points, choices (ids and all), and correct
    // come back from the template; the local edit is gone.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["id"], qid, "the question keeps its id");
    assert_eq!(res.body["text"], "capital of France?");
    assert_eq!(res.body["points"], 7);
    assert_eq!(res.body["subject"], subject, "the exam-side subject stays");
    assert_eq!(res.body["from_bank"], bid, "provenance survives");
    assert_eq!(
        choice_of(&res.body, 0),
        keep_choice,
        "template choice ids adopted"
    );
    assert_eq!(choice_of(&res.body, 1), fresh_choice);
    assert_eq!(res.body["choices"][1]["text"], "Marseille");
    assert_eq!(res.body["correct"], keep_choice);
    // Pictures match the template: the new illustration landed, and the
    // dropped option's picture is gone rather than orphaned on a stale slot.
    assert_eq!(
        res.body["image"]["content_type"], "image/png",
        "{}",
        res.body
    );
    assert!(res.body["choice_images"][0].is_null(), "{}", res.body);
    assert!(res.body["choice_images"][1].is_null(), "{}", res.body);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        bytes, b"new-illustration",
        "the blob was re-copied, not kept"
    );
    // No orphan blob rows left behind by the dropped option.
    assert_eq!(image_blob_keys(&db).await.len(), 1);

    // A hand-authored question has nothing to refresh from — 400, not a 404
    // that would read as "your exam is gone".
    let plain = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "mine alone", "kind": "text", "points": 1 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{plain}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Once a student sits an exam, its questions freeze — the refresh answers
    // to the same guard as every other question mutation.
    let now = Timestamp::now().as_millis();
    let live = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "live", "kind": "final", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let live_qid = id_of(&res.body);
    let student = login(&app, "bref_s").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &teacher, &course, &student_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/questions/{live_qid}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Deleting the template clears `from_bank`, so the copy has nothing left to
    // refresh from — the action stops existing rather than half-working.
    let res = send(
        &app,
        "DELETE",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    let refreshed = common::items(&res.body)
        .iter()
        .find(|item| id_of(item) == qid)
        .cloned()
        .unwrap();
    assert!(refreshed["from_bank"].is_null(), "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// A course PATCH is a field-scoped write. The handler reads the course, then
/// awaits a term lookup (and its `TERM_LOCK`) before saving, holding nothing
/// over the course row itself — so a manager's teacher assignment can land in
/// that window. A whole-row save from the stale struct would silently revert
/// the staffing. Driven at the domain level: HTTP offers no way to interleave
/// inside the handler.
#[tokio::test]
async fn a_course_patch_does_not_clobber_a_concurrent_teacher_assignment() {
    use hezarfen_backend::domain::course::{
        Course, CourseDescription, CourseId, CourseKind, CourseTitle,
    };

    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "cclob_o", "teacher").await;
    let helper = login_as(&app, &db, "cclob_h", "teacher").await;
    let boss = login_as(&app, &db, "cclob_m", "manager").await;
    let helper_id = me_id(&app, &helper).await;
    let course = create_course(&app, &owner, "cclob_c").await;

    // The handler's read, then the assignment lands mid-window.
    let stale = Course::read(&CourseId::from_key(&course), &db)
        .await
        .unwrap()
        .expect("course exists");
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&boss),
        Some(json!({ "user_id": helper_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let updated = stale
        .update(
            Some(CourseTitle::try_new("renamed").unwrap()),
            Some(CourseDescription::try_new("").unwrap()),
            Some(CourseKind::course()),
            None,
            None,
            &db,
        )
        .await
        .expect("update written");
    assert_eq!(
        updated.get_teachers().len(),
        1,
        "the assignment was reverted"
    );

    // The rename landed and the assigned teacher stayed assigned.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.body["title"], "renamed");
    assert_eq!(res.body["teachers"][0]["username"], "cclob_h");
}

/// The mirror image: staffing writes only `teachers`. `assign_teacher` reads
/// the course, then awaits the target user's row (a role check) before saving,
/// so a course PATCH can land in between — and a whole-row save would revert
/// the rename. `unassign_teacher` gets the same field-scoped treatment.
#[tokio::test]
async fn staffing_a_course_does_not_clobber_a_concurrent_edit() {
    use hezarfen_backend::domain::course::{Course, CourseId};
    use hezarfen_backend::domain::user::UserId;

    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "cstaff_o", "teacher").await;
    let helper = login_as(&app, &db, "cstaff_h", "teacher").await;
    let helper_id = me_id(&app, &helper).await;
    let course = create_course(&app, &owner, "cstaff_c").await;

    // The handler's read, then someone else's edit lands mid-window.
    let stale = Course::read(&CourseId::from_key(&course), &db)
        .await
        .unwrap()
        .expect("course exists");
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&owner),
        Some(json!({ "title": "edited by someone else", "capacity": 9 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let assigned = stale
        .assign_teacher(&UserId::from_key(&helper_id), &db)
        .await
        .expect("assignment written");
    assert_eq!(assigned.get_title().as_str(), "edited by someone else");
    assert_eq!(assigned.get_capacity(), Some(9));
    assert_eq!(assigned.get_teachers().len(), 1);

    // Unassigning from a struct read before another edit is just as safe.
    let stale = Course::read(&CourseId::from_key(&course), &db)
        .await
        .unwrap()
        .expect("course exists");
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&owner),
        Some(json!({ "title": "edited again" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let dropped = stale
        .unassign_teacher(&UserId::from_key(&helper_id), &db)
        .await
        .expect("unassignment written")
        .expect("teacher was assigned");
    assert_eq!(dropped.get_title().as_str(), "edited again");
    assert!(dropped.get_teachers().is_empty());

    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.body["title"], "edited again");
    assert_eq!(res.body["capacity"], 9);
    assert_eq!(res.body["teachers"].as_array().unwrap().len(), 0);
}

// --- schedule-window list filters ----------------------------------------

/// Titles of a list envelope, in response order.
fn titles(body: &serde_json::Value) -> Vec<String> {
    common::items(body)
        .iter()
        .map(|item| item["title"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn events_schedule_window_filters_and_orders() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let now = Timestamp::now().as_millis();

    // Four events, created oldest-id first: one with no schedule at all, one
    // already running (inside the 60s backdating grace), one starting later
    // with no end, and one that only carries an end.
    for body in [
        json!({ "title": "no-schedule", "description": "" }),
        json!({ "title": "running", "starts_at": now - 30_000, "ends_at": now + 600_000 }),
        json!({ "title": "later", "starts_at": now + 900_000 }),
        json!({ "title": "ending", "ends_at": now + 300_000 }),
    ] {
        let res = send(&app, "POST", "/events", Some(&ali), Some(body)).await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    // No window params: unchanged behaviour — every row, newest-created first.
    let all = send(&app, "GET", "/events", Some(&ali), None).await;
    assert_eq!(common::total(&all.body), 4);
    assert_eq!(
        titles(&all.body),
        ["ending", "later", "running", "no-schedule"]
    );

    // ends_after: keeps unfinished rows (running on ends_at, later on the
    // starts_at fallback), drops the schedule-less one, ascending by schedule.
    let res = send(
        &app,
        "GET",
        &format!("/events?ends_after={now}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(titles(&res.body), ["running", "ending", "later"]);

    // Past the running event's end: only the pure-end row and the fallback row.
    let res = send(
        &app,
        "GET",
        &format!("/events?ends_after={}", now + 700_000),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(titles(&res.body), ["later"]);

    // starts_after excludes the already-started row and every start-less row.
    let res = send(
        &app,
        "GET",
        &format!("/events?starts_after={now}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(titles(&res.body), ["later"]);

    // total is the filtered, pre-pagination count; the page is the soonest.
    let res = send(
        &app,
        "GET",
        &format!("/events?ends_after={now}&limit=1"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(titles(&res.body), ["running"]);

    // Invalid values are a 400 naming the field.
    for query in ["starts_after=-1", "ends_after=-1", "ends_after=soon"] {
        let res = send(&app, "GET", &format!("/events?{query}"), Some(&ali), None).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{query}");
    }
}

#[tokio::test]
async fn exams_schedule_window_applies_after_visibility_and_drafts() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ayse = login_as(&app, &db, "ayse", "student").await;
    let now = Timestamp::now().as_millis();

    let course = create_course(&app, &ali, "math").await;
    let other = create_course(&app, &ali, "hidden").await;
    enroll(&app, &ali, &course, &me_id(&app, &ayse).await).await;

    // Windowed exams in the student's course, plus a draft, an open-mode exam
    // (no window at all), and one in a course they cannot see.
    for (course, body) in [
        (
            &course,
            json!({ "title": "soon", "kind": "quiz", "mode": "sync",
                          "starts_at": now + 600_000, "ends_at": now + 900_000 }),
        ),
        (
            &course,
            json!({ "title": "late", "kind": "quiz", "mode": "sync",
                          "starts_at": now + 1_800_000, "ends_at": now + 2_400_000 }),
        ),
        (
            &course,
            json!({ "title": "draft", "kind": "quiz", "mode": "sync", "draft": true,
                          "starts_at": now + 60_000, "ends_at": now + 120_000 }),
        ),
        (
            &course,
            json!({ "title": "open", "kind": "quiz", "mode": "open" }),
        ),
        (
            &other,
            json!({ "title": "unseen", "kind": "quiz", "mode": "sync",
                         "starts_at": now + 60_000, "ends_at": now + 120_000 }),
        ),
    ] {
        let res = create_exam_with(&app, &ali, course, body).await;
        assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    }

    // Unfiltered: the student sees their course's non-draft exams only.
    let all = send(&app, "GET", "/exams", Some(&ayse), None).await;
    assert_eq!(common::total(&all.body), 3);
    assert_eq!(titles(&all.body), ["open", "late", "soon"]);

    // Windowed: the draft and the other course stay invisible (the window runs
    // after visibility), the window-less open exam drops out, soonest first.
    let res = send(
        &app,
        "GET",
        &format!("/exams?ends_after={now}"),
        Some(&ayse),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2);
    assert_eq!(titles(&res.body), ["soon", "late"]);

    // The teacher's own draft is windowed like any other exam of theirs.
    let res = send(
        &app,
        "GET",
        &format!("/exams?starts_after={now}&limit=2"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 4);
    assert_eq!(titles(&res.body), ["draft", "unseen"]);

    let res = send(&app, "GET", "/exams?starts_after=-1", Some(&ayse), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}
