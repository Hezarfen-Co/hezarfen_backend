//! Shared helpers for the router-level test binaries (`integration`, `persistence`).
//! Drives an `axum::Router` in-process via `tower::ServiceExt::oneshot`.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

pub struct Res {
    pub status: StatusCode,
    pub body: Value,
    /// The `session=<token>` pair, if the response set one.
    pub cookie: Option<String>,
}

/// One shared blob directory for every in-memory app in this test process.
/// Blob names are per-row ULIDs, so apps never collide; the static keeps the
/// `TempDir` (and so the directory) alive for the whole run.
static FILES_DIR: OnceLock<TempDir> = OnceLock::new();

pub fn files_dir() -> PathBuf {
    FILES_DIR
        .get_or_init(|| tempfile::tempdir().expect("files tempdir"))
        .path()
        .to_path_buf()
}

/// A router plus a handle to its (shared) in-memory database. Tests that need to
/// grant roles use the handle to seed them directly — the app's only role
/// bootstrap path is out-of-band, exactly like production's manual SQL.
pub async fn app_and_db() -> (Router, Database) {
    let db = database::init_mem().await.expect("in-memory db");
    let app = build_router(AppState {
        db: db.clone(),
        files_path: files_dir(),
        cookie_secure: false,
        // Off, so suites hammering the API never trip a limit; the dedicated
        // `rate_limit` test binary opts into tight configs on purpose.
        rate_limit: RateLimitConfig::unlimited(),
        chat_limit: Default::default(),
        exam_presence: Default::default(),
        db_up: Default::default(),
        ai: None,
    });
    (app, db)
}

/// A router backed by a fresh in-memory database.
pub async fn mem_app() -> Router {
    app_and_db().await.0
}

/// Force `username`'s role, mirroring the manual `UPDATE user SET role=...`
/// bootstrap. Roles are read fresh per request, so this takes effect at once.
pub async fn set_role(db: &Database, username: &str, role: &str) {
    db.query("UPDATE user SET role = $role WHERE username = $u")
        .bind(("role", role.to_string()))
        .bind(("u", username.to_string()))
        .await
        .expect("set_role query")
        .check()
        .expect("set_role check");
}

/// Register (password `secret1`), promote to `role`, then log in. Returns the
/// session `Cookie` value.
pub async fn login_as(app: &Router, db: &Database, username: &str, role: &str) -> String {
    let creds = json!({ "username": username, "password": "secret1" });
    let reg = send(app, "POST", "/auth/register", None, Some(creds.clone())).await;
    assert_eq!(reg.status, StatusCode::CREATED, "register {username}");
    set_role(db, username, role).await;
    let res = send(app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::OK, "login {username}");
    res.cookie.expect("session cookie set on login")
}

/// Send one request through `app`. `cookie` is a raw `Cookie` header value.
pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> Res {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    let request = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .map(|s| s.to_string());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    Res {
        status,
        body,
        cookie,
    }
}

/// Send one request with a raw body and content type; returns the raw
/// response (status, headers, body bytes) — for driving the file endpoints.
pub async fn send_raw(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, bytes)
}

const MULTIPART_BOUNDARY: &str = "hezarfen-test-boundary";

/// A `multipart/form-data` body with a single `file` part.
pub fn multipart_file(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    body
}

/// Upload `bytes` as a file onto `note` (no assertion). Parses the JSON body
/// like `send`.
pub async fn upload_file(
    app: &Router,
    cookie: &str,
    note: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Res {
    let (status, _, body) = send_raw(
        app,
        "POST",
        &format!("/notes/{note}/files"),
        Some(cookie),
        Some(&format!(
            "multipart/form-data; boundary={MULTIPART_BOUNDARY}"
        )),
        multipart_file(filename, content_type, bytes),
    )
    .await;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    Res {
        status,
        body,
        cookie: None,
    }
}

/// Register (password `secret1`) then log in; returns the session `Cookie` value.
pub async fn login(app: &Router, username: &str) -> String {
    let creds = json!({ "username": username, "password": "secret1" });
    let reg = send(app, "POST", "/auth/register", None, Some(creds.clone())).await;
    assert_eq!(reg.status, StatusCode::CREATED, "register {username}");
    let res = send(app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::OK, "login {username}");
    res.cookie.expect("session cookie set on login")
}

pub fn id_of(v: &Value) -> String {
    v["id"].as_str().expect("id field").to_string()
}

/// The `items` array of a paginated list envelope (`{items, total, limit,
/// offset}`) — the shape every `GET` list endpoint returns. Panics if `v`
/// isn't such an envelope, so a test that points it at the wrong body fails
/// loudly.
pub fn items(v: &Value) -> &Vec<Value> {
    v["items"]
        .as_array()
        .unwrap_or_else(|| panic!("expected a paginated list envelope with items[], got {v}"))
}

/// The `total` count from a paginated list envelope.
pub fn total(v: &Value) -> i64 {
    v["total"].as_i64().expect("paginated envelope total")
}

/// The caller's own user id via `GET /auth/me`.
pub async fn me_id(app: &Router, cookie: &str) -> String {
    let res = send(app, "GET", "/auth/me", Some(cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "GET /auth/me");
    id_of(&res.body)
}

/// Create a course as `cookie` (asserts 201); returns its id.
pub async fn create_course(app: &Router, cookie: &str, title: &str) -> String {
    let res = send(
        app,
        "POST",
        "/courses",
        Some(cookie),
        Some(json!({ "title": title })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create course {title}");
    id_of(&res.body)
}

/// Create a subject inside `course` as `cookie` (asserts 201); returns its id.
/// Every exam question must be tagged with one of its course's subjects.
pub async fn create_subject(app: &Router, cookie: &str, course: &str, name: &str) -> String {
    let res = send(
        app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(cookie),
        Some(json!({ "name": name })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create subject {name}");
    id_of(&res.body)
}

/// Create an exam inside `course` as `cookie` (asserts 201); returns its id.
/// The exam's weight in the course average comes from its kind (settings).
pub async fn create_exam(
    app: &Router,
    cookie: &str,
    course: &str,
    title: &str,
    kind: &str,
) -> String {
    let res = send(
        app,
        "POST",
        &format!("/courses/{course}/exams"),
        Some(cookie),
        Some(json!({ "title": title, "kind": kind })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create exam {title}");
    id_of(&res.body)
}

/// Create an exam inside `course` from a full JSON body (no assertion) —
/// for exercising the scheduling fields and their validation.
pub async fn create_exam_with(app: &Router, cookie: &str, course: &str, body: Value) -> Res {
    send(
        app,
        "POST",
        &format!("/courses/{course}/exams"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// Create a homework inside `course` as `cookie` (asserts 201); returns its
/// id. Tagged with `subject` (required — every homework carries one of its
/// course's subjects) and due at `due_at` (unix-millis; must not lie past the
/// 60s grace). Whole-course audience; use [`create_homework_with`] for an
/// `assigned` subset.
pub async fn create_homework(
    app: &Router,
    cookie: &str,
    course: &str,
    subject: &str,
    title: &str,
    due_at: i64,
) -> String {
    let res = send(
        app,
        "POST",
        &format!("/courses/{course}/homework"),
        Some(cookie),
        Some(json!({ "title": title, "subject_id": subject, "due_at": due_at })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create homework {title}");
    id_of(&res.body)
}

/// Create a homework inside `course` from a full JSON body (no assertion) —
/// for exercising the `assigned` subset and the validation rejects.
pub async fn create_homework_with(app: &Router, cookie: &str, course: &str, body: Value) -> Res {
    send(
        app,
        "POST",
        &format!("/courses/{course}/homework"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// Create a lesson session inside `course` as `cookie` (asserts 201); returns
/// its id. The session's teacher defaults to the caller.
pub async fn create_session(app: &Router, cookie: &str, course: &str, starts_at: i64) -> String {
    let res = send(
        app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(cookie),
        Some(json!({ "starts_at": starts_at })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create session");
    id_of(&res.body)
}

/// Drop `user_id` from `course`'s roster as `cookie` (asserts 204). Deleting a
/// course is refused while anyone is still enrolled, so cascade tests empty the
/// roster first.
pub async fn unenroll(app: &Router, cookie: &str, course: &str, user_id: &str) {
    let res = send(
        app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{user_id}"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "unenroll {user_id}");
}

/// Enroll `user_id` into `course` as `cookie` (asserts 200).
pub async fn enroll(app: &Router, cookie: &str, course: &str, user_id: &str) {
    let res = send(
        app,
        "POST",
        &format!("/courses/{course}/enrollments"),
        Some(cookie),
        Some(json!({ "user_id": user_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "enroll {user_id}");
}
