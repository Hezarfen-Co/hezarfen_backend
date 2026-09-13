//! Shared helpers for the router-level test binaries (`integration`,
//! `persistence`, and friends). Drives an `axum::Router` in-process via
//! `tower::ServiceExt::oneshot`, over a **per-test pair of Postgres
//! databases** — control plus demo school, cloned from the shared school
//! template by `database::init_test_tenants`, dropped with the test.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use hezarfen_backend::ai::AiBridge;
use hezarfen_backend::database::Database;
use hezarfen_backend::module::ModuleSet;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::telemetry::Metrics;
use hezarfen_backend::tenant::{DEMO_SLUG, Slug, Tenants};
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

/// A well-formed id for a row that exists nowhere — the canonical ghost
/// parent every FK-refusal test aims at. Replaces the ULID literal
/// `01J8XZ0K3Q8G7X2M4N5P6R7S8T` the suites used before the UUID v7 port.
pub const GHOST_ID: &str = "019732e3-7b00-7000-8000-00000000dead";

/// A second well-formed, never-minted id, for "look this up, expect a miss".
/// Replaces `01ZZZZZZZZZZZZZZZZZZZZZZZZ`.
pub const ABSENT_ID: &str = "019732e3-7b00-7000-8000-00000000eeee";

/// One shared blob directory for every app in this test process. Blob names
/// are per-row ids, so apps never collide; the static keeps the `TempDir`
/// (and so the directory) alive for the whole run.
static FILES_DIR: OnceLock<TempDir> = OnceLock::new();

pub fn files_dir() -> PathBuf {
    FILES_DIR
        .get_or_init(|| tempfile::tempdir().expect("files tempdir"))
        .path()
        .to_path_buf()
}

/// A router plus a handle to its demo school's database. Tests that need to
/// grant roles use the handle to seed them directly — the app's only role
/// bootstrap path is out-of-band, exactly like production's manual SQL.
pub async fn app_and_db() -> (Router, Database) {
    app_with_ai(None).await
}

/// [`app_and_db`] plus the registry behind it, for the suites whose subject is
/// tenancy itself (a second school, a suspension, the builder surface).
pub async fn app_and_tenants() -> (Router, Database, Tenants) {
    let (app, db, tenants) = app_parts(None, Metrics::noop()).await;
    (app, db, tenants)
}

/// Where the demo school's blobs land: one directory per school under
/// `FILES_PATH`, created on the school's first upload.
pub fn blob_dir() -> PathBuf {
    let dir = files_dir().join(DEMO_SLUG);
    std::fs::create_dir_all(&dir).expect("blob dir");
    dir
}

/// The raw session token out of a `session=<school>.<token>` cookie value —
/// what a test needs when it writes a session row by hand.
pub fn cookie_token(cookie: &str) -> &str {
    cookie
        .trim_start_matches("session=")
        .split_once('.')
        .expect("a session cookie carries its school")
        .1
}

/// A test deployment: the registry, plus the demo school's handle. For the
/// suites that build their own `AppState` instead of using [`app_and_db`].
/// (The name outlived the in-memory engine; the shape is what the suites
/// bind to.)
pub async fn mem_deployment() -> (Tenants, Database) {
    let tenants = database::init_test_tenants().await;
    let db = demo_db(&tenants).await;
    (tenants, db)
}

/// The demo school's handle out of a registry.
pub async fn demo_db(tenants: &Tenants) -> Database {
    tenants
        .get(&Slug::try_new(DEMO_SLUG).expect("the demo slug"))
        .await
        .expect("the demo school resolves")
}

/// The same router, with the AI bridge wired into its state. `build_router`
/// arms the bridge's api-read handle whenever `ai` is `Some`, so this is the
/// bootstrap that makes a QUIC service's reads dispatch into a real router.
pub async fn app_with_ai(ai: Option<AiBridge>) -> (Router, Database) {
    let (app, db, _) = app_parts(ai, Metrics::noop()).await;
    (app, db)
}

/// [`app_with_ai`] plus the registry behind it, for the suites whose subject is
/// an AI service reading *across* schools.
pub async fn app_with_ai_tenants(ai: Option<AiBridge>) -> (Router, Database, Tenants) {
    app_parts(ai, Metrics::noop()).await
}

/// The one bootstrap: a fresh test deployment (control database + the demo
/// school), a router over it, and the demo school's handle — which is what
/// every suite means by "the database", since that is where users and rows
/// live.
async fn app_parts(ai: Option<AiBridge>, metrics: Metrics) -> (Router, Database, Tenants) {
    let tenants = database::init_test_tenants().await;
    let db = demo_db(&tenants).await;
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants: tenants.clone(),
        files_path: files_dir(),
        cookie_secure: false,
        // Off, so suites hammering the API never trip a limit; the dedicated
        // `rate_limit` test binary opts into tight configs on purpose.
        rate_limit: RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai,
        metrics,
    });
    (app, db, tenants)
}

/// The same router with the caller's instruments, so a test can read back what
/// the HTTP edge recorded without touching the process-global meter provider.
pub async fn app_with_metrics(metrics: Metrics) -> (Router, Database) {
    let (app, db, _) = app_parts(None, metrics).await;
    (app, db)
}

/// Attribute keys that would carry personal data out of the process. KVKK
/// analysis fixed this list: telemetry names the school and the route, never
/// the person, their address, or what they sent. Shared so the HTTP edge
/// (`telemetry.rs`) and the AI bridge (`ai_bridge.rs`) are held to one list.
///
/// `server.address` is deliberately absent: that is our own bind address, not
/// a caller's.
pub const FORBIDDEN_TELEMETRY_KEYS: &[&str] = &[
    "url.path",
    "url.full",
    "url.query",
    "client.address",
    "network.peer.address",
    "user_agent.original",
    "user.id",
    "user.name",
    "username",
    "email",
    "enduser.id",
    "cookie",
    "http.request.body",
    "http.response.body",
];

/// [`FORBIDDEN_TELEMETRY_KEYS`] plus the two header prefixes — any captured
/// request or response header is personal data by default.
pub fn is_forbidden_key(key: &str) -> bool {
    FORBIDDEN_TELEMETRY_KEYS.contains(&key)
        || key.starts_with("http.request.header.")
        || key.starts_with("http.response.header.")
}

/// A router backed by a fresh per-test Postgres deployment.
pub async fn mem_app() -> Router {
    app_and_db().await.0
}

/// Force `username`'s role, mirroring the manual `UPDATE app_user SET
/// role=...` bootstrap. Roles are read fresh per request, so this takes
/// effect at once.
pub async fn set_role(db: &Database, username: &str, role: &str) {
    sqlx::query("UPDATE app_user SET role = $2 WHERE username = $1")
        .bind(username)
        .bind(role)
        .execute(db)
        .await
        .expect("set_role query");
}

/// Register (password `secret1`), promote to `role`, then log in. Returns the
/// session `Cookie` value.
pub async fn login_as(app: &Router, db: &Database, username: &str, role: &str) -> String {
    login_as_school(app, db, DEMO_SLUG, username, role).await
}

/// [`login_as`] in a named school. `db` must be that school's own handle — the
/// role is written where the user was created, which is what makes two schools
/// able to hold the same username independently.
pub async fn login_as_school(
    app: &Router,
    db: &Database,
    school: &str,
    username: &str,
    role: &str,
) -> String {
    let reg = send(
        app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": school, "username": username, "password": "secret1" })),
    )
    .await;
    assert_eq!(reg.status, StatusCode::CREATED, "register {username}");
    set_role(db, username, role).await;
    let res = send(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": username, "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "login {username}");
    // The account is a person: if `username` already belongs to another
    // school, login answers a choice list and never names a school — bind
    // the school this helper was called for.
    let cookie = if res.body["schools"].is_array() {
        let selected = send(
            app,
            "POST",
            "/auth/school",
            res.cookie.as_deref(),
            Some(json!({ "school": school })),
        )
        .await;
        assert_eq!(
            selected.status,
            StatusCode::OK,
            "select {school} for {username}: {}",
            selected.body
        );
        selected.cookie.expect("school cookie after selection")
    } else {
        res.cookie.expect("session cookie set on login")
    };
    cookie
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

/// Upload `bytes` as a multipart file to `uri` (no assertion). Parses the
/// JSON body like `send`.
pub async fn upload_file_at(
    app: &Router,
    cookie: &str,
    uri: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Res {
    let (status, _, body) = send_raw(
        app,
        "POST",
        uri,
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

/// Upload `bytes` as a file onto a personal `note` (no assertion).
pub async fn upload_file(
    app: &Router,
    cookie: &str,
    note: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Res {
    upload_file_at(
        app,
        cookie,
        &format!("/notes/{note}/files"),
        filename,
        content_type,
        bytes,
    )
    .await
}

/// Upload `bytes` as a file onto a course `note` (no assertion).
pub async fn upload_course_note_file(
    app: &Router,
    cookie: &str,
    note: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> Res {
    upload_file_at(
        app,
        cookie,
        &format!("/course-notes/{note}/files"),
        filename,
        content_type,
        bytes,
    )
    .await
}

/// Register (password `secret1`) then log in; returns the session `Cookie` value.
pub async fn login(app: &Router, username: &str) -> String {
    let reg = send(
        app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": DEMO_SLUG, "username": username, "password": "secret1" })),
    )
    .await;
    assert_eq!(reg.status, StatusCode::CREATED, "register {username}");
    let res = send(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": username, "password": "secret1" })),
    )
    .await;
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

// ---- multi-school deployments ---------------------------------------------
//
// The probes whose subject is tenancy itself — cross-school isolation,
// suspension, school deletion — build a deployment with more schools than
// the demo one. Under database-per-school tenancy there is no separate
// "remote" mode to exercise: production *is* this shape, and a second
// deployment is a second `database::init_test_tenants()` away. (The
// `surreal start` remote lane this section used to hold died with the
// embedded engine.)

/// A deployment with its own blob directory: what the tenancy probes hold.
pub struct TestDeployment {
    pub app: Router,
    pub tenants: Tenants,
    /// `FILES_PATH` — this deployment's own directory, never the shared
    /// [`files_dir`], so one deployment's uploads cannot be counted by
    /// another's probes.
    pub files: PathBuf,
    _files: TempDir,
}

/// [`app_and_tenants`] with named schools created alongside the demo — where
/// the former `remote_deployment` probes re-point. The named schools go
/// through the real [`Tenants::create`], since the create path is part of
/// what the tenancy probes examine.
pub async fn deployment_with(schools: &[(&str, &str)]) -> TestDeployment {
    let files = tempfile::tempdir().expect("files tempdir");
    let tenants = database::init_test_tenants().await;
    for (slug, name) in schools {
        tenants
            .create(
                &Slug::try_new(slug).expect("a school slug"),
                name,
                ModuleSet::all(),
            )
            .await
            .unwrap_or_else(|err| panic!("create school {slug}: {err}"));
    }
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants: tenants.clone(),
        files_path: files.path().to_path_buf(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai: None,
        metrics: Metrics::noop(),
    });
    TestDeployment {
        app,
        tenants,
        files: files.path().to_path_buf(),
        _files: files,
    }
}
