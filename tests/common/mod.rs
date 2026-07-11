//! Shared helpers for the router-level test binaries (`integration`, `persistence`).
//! Drives an `axum::Router` in-process via `tower::ServiceExt::oneshot`.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::{Value, json};
use tower::ServiceExt;

pub struct Res {
    pub status: StatusCode,
    pub body: Value,
    /// The `session=<token>` pair, if the response set one.
    pub cookie: Option<String>,
}

/// A router plus a handle to its (shared) in-memory database. Tests that need to
/// grant roles use the handle to seed them directly — the app's only role
/// bootstrap path is out-of-band, exactly like production's manual SQL.
pub async fn app_and_db() -> (Router, Database) {
    let db = database::init_mem().await.expect("in-memory db");
    let app = build_router(AppState {
        db: db.clone(),
        cookie_secure: false,
        // Off, so suites hammering the API never trip a limit; the dedicated
        // `rate_limit` test binary opts into tight configs on purpose.
        rate_limit: RateLimitConfig::unlimited(),
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
