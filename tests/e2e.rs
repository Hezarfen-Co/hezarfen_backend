//! End-to-end tests: boot the real server on an ephemeral TCP port and drive it
//! with `reqwest` over HTTP, using its cookie jar exactly like a browser client.

use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

/// Start the server on a random port. Returns its base URL (e.g.
/// `http://127.0.0.1:54321`) plus a handle to its database, so a test can grant
/// roles the same out-of-band way production does.
async fn spawn_server() -> (String, Database) {
    let db = database::init_mem().await.expect("in-memory db");
    let app = build_router(AppState {
        db: db.clone(),
        cookie_secure: false,
        // Every request here comes from 127.0.0.1, so per-IP limits would
        // meter the whole suite as one client. Off; `rate_limit.rs` covers it.
        rate_limit: RateLimitConfig::unlimited(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Mirror `main`: expose peer addresses so the limiter key path in
        // production is the same one exercised end-to-end.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (format!("http://{addr}"), db)
}

/// Grant `user` a role directly (the manual bootstrap path).
async fn promote(db: &Database, user: &str, role: &str) {
    db.query("UPDATE user SET role = $r WHERE username = $u")
        .bind(("r", role.to_string()))
        .bind(("u", user.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// A reqwest client with its own cookie jar (one per simulated user).
fn client() -> Client {
    Client::builder().cookie_store(true).build().unwrap()
}

async fn register(client: &Client, base: &str, user: &str) -> reqwest::Response {
    client
        .post(format!("{base}/auth/register"))
        .json(&json!({ "username": user, "password": "secret1" }))
        .send()
        .await
        .unwrap()
}

async fn login(client: &Client, base: &str, user: &str) {
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&json!({ "username": user, "password": "secret1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "login {user}");
}

#[tokio::test]
async fn full_user_journey() {
    let (base, db) = spawn_server().await;
    let ali = client();
    let veli = client();

    // --- signup + auth ---------------------------------------------------
    assert_eq!(
        register(&ali, &base, "ali").await.status(),
        StatusCode::CREATED
    );
    assert_eq!(
        register(&veli, &base, "veli").await.status(),
        StatusCode::CREATED
    );
    // ali runs the class: give her the teacher role. veli stays a student.
    promote(&db, "ali", "teacher").await;

    // me before login -> 401
    let res = ali.get(format!("{base}/auth/me")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    login(&ali, &base, "ali").await;
    login(&veli, &base, "veli").await;

    let me: Value = ali
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["username"], "ali");

    let veli_id = veli
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // --- notes -----------------------------------------------------------
    let note: Value = ali
        .post(format!("{base}/notes"))
        .json(&json!({ "title": "first", "content": "hello" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let note_id = note["id"].as_str().unwrap().to_string();
    assert_eq!(note["title"], "first");

    // veli cannot read ali's note
    let res = veli
        .get(format!("{base}/notes/{note_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // --- events + attendance --------------------------------------------
    let event: Value = ali
        .post(format!("{base}/events"))
        .json(&json!({ "title": "standup" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let event_id = event["id"].as_str().unwrap().to_string();

    // ali marks veli present
    let res = ali
        .post(format!("{base}/events/{event_id}/attendance"))
        .json(&json!({ "status": "present", "user_id": veli_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let roster: Value = veli
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(roster.as_array().unwrap().len(), 1);
    assert_eq!(roster[0]["user"], veli_id);
    assert_eq!(roster[0]["status"], "present");

    // veli is only a student, so she cannot create events.
    let res = veli
        .post(format!("{base}/events"))
        .json(&json!({ "title": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // --- logout invalidates the session ---------------------------------
    let res = ali
        .post(format!("{base}/auth/logout"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    let res = ali.get(format!("{base}/auth/me")).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn attendance_rollup_across_users() {
    let (base, db) = spawn_server().await;
    let host = client();
    let a = client();
    let b = client();

    for (c, name) in [(&host, "host"), (&a, "amy"), (&b, "ben")] {
        assert_eq!(register(c, &base, name).await.status(), StatusCode::CREATED);
        login(c, &base, name).await;
    }
    // The host runs the event and records everyone, which is a teacher+ action.
    promote(&db, "host", "teacher").await;

    let id_of = |v: &Value| v["id"].as_str().unwrap().to_string();
    let amy_id = id_of(
        &a.get(format!("{base}/auth/me"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
    );
    let ben_id = id_of(
        &b.get(format!("{base}/auth/me"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
    );

    // Host creates an event and records everyone.
    let event_id = id_of(
        &host
            .post(format!("{base}/events"))
            .json(&json!({ "title": "all-hands" }))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
    );

    for (uid, status) in [(&amy_id, "present"), (&ben_id, "late")] {
        let res = host
            .post(format!("{base}/events/{event_id}/attendance"))
            .json(&json!({ "status": status, "user_id": uid }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
    // Host marks self too.
    host.post(format!("{base}/events/{event_id}/attendance"))
        .json(&json!({ "status": "present" }))
        .send()
        .await
        .unwrap();

    // Any attendee can read the full roster (3 people).
    let roster: Value = b
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(roster.as_array().unwrap().len(), 3);

    // Unauthenticated client is refused.
    let res = client()
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
