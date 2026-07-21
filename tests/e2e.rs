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
        // Kept (not auto-deleted) so the directory outlives this helper;
        // it's under the OS temp dir, reclaimed like any other temp file.
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        // Every request here comes from 127.0.0.1, so per-IP limits would
        // meter the whole suite as one client. Off; `rate_limit.rs` covers it.
        rate_limit: RateLimitConfig::unlimited(),
        exam_presence: Default::default(),
        db_up: Default::default(),
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

    // The roster is a teacher+ view — veli (a student) is refused …
    let res = veli
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // … while ali (the teacher) reads it in full.
    let roster: Value = ali
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(roster["items"].as_array().unwrap().len(), 1);
    // People come back as refs — id plus something a human can read.
    assert_eq!(roster["items"][0]["user"]["id"], veli_id);
    assert_eq!(roster["items"][0]["user"]["username"], "veli");
    assert_eq!(roster["items"][0]["marked_by"]["username"], "ali");
    assert_eq!(roster["items"][0]["status"], "present");

    // --- user search (for the pickers) -----------------------------------
    // Teacher+ finds people by fragment; the refs carry no contact details.
    let found: Value = ali
        .get(format!("{base}/users/search?q=vel"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["items"][0]["username"], "veli");
    assert!(found["items"][0].get("email").is_none());

    // Role filter narrows: veli is a student, so she matches `role=student`
    // but vanishes under `role=teacher`. An unknown role is a 400.
    let found: Value = ali
        .get(format!("{base}/users/search?q=vel&role=student"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
    assert_eq!(found["items"][0]["username"], "veli");

    let found: Value = ali
        .get(format!("{base}/users/search?q=vel&role=teacher"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(found["items"].as_array().unwrap().is_empty());

    let res = ali
        .get(format!("{base}/users/search?q=vel&role=wizard"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // The filter also *includes* on non-student roles: ali is the teacher.
    let found: Value = ali
        .get(format!("{base}/users/search?q=al&role=teacher"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
    assert_eq!(found["items"][0]["username"], "ali");

    // Search matches profile names too, and the role filter applies to those
    // hits as well: "lic" only exists in ali's freshly set name, not in any
    // username, so it appears under `role=teacher` and not `role=student`.
    let res = ali
        .patch(format!("{base}/users/me"))
        .json(&json!({ "name": "Alice" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let found: Value = ali
        .get(format!("{base}/users/search?q=lic&role=teacher"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
    assert_eq!(found["items"][0]["username"], "ali");
    assert_eq!(found["items"][0]["display_name"], "Alice");

    let found: Value = ali
        .get(format!("{base}/users/search?q=lic&role=student"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(found["items"].as_array().unwrap().is_empty());

    let res = veli
        .get(format!("{base}/users/search?q=ali"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

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

    // The roster is a teacher+ view: an attendee sees only their own tallies
    // (`/attendance/me`), so ben is refused …
    let res = b
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // … while the host reads the full roster (3 people).
    let roster: Value = host
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(roster["items"].as_array().unwrap().len(), 3);

    // Unauthenticated client is refused.
    let res = client()
        .get(format!("{base}/events/{event_id}/attendance"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// The live exam monitor over a real TCP connection: an `EventSource`-style
/// GET must yield a `snapshot` event carrying the roster within the first
/// ticks (the first one fires immediately on connect).
#[tokio::test]
async fn live_exam_stream_pushes_snapshots_over_http() {
    let (base, db) = spawn_server().await;
    let teacher = client();
    register(&teacher, &base, "hoca").await;
    promote(&db, "hoca", "teacher").await;
    login(&teacher, &base, "hoca").await;

    let student = client();
    register(&student, &base, "veli").await;
    login(&student, &base, "veli").await;
    let student_id: Value = student
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let student_id = student_id["id"].as_str().unwrap().to_string();

    // Course + enrolled student + a sync exam whose window is open now
    // (window times judged by the server clock, fetched from `/time`).
    let now: Value = teacher
        .get(format!("{base}/time"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let now = now["now"].as_i64().unwrap();

    let course: Value = teacher
        .post(format!("{base}/courses"))
        .json(&json!({ "title": "algebra" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let course_id = course["id"].as_str().unwrap();
    let res = teacher
        .post(format!("{base}/courses/{course_id}/enrollments"))
        .json(&json!({ "user_id": student_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let exam: Value = teacher
        .post(format!("{base}/courses/{course_id}/exams"))
        .json(&json!({
            "title": "final", "kind": "final",
            "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exam_id = exam["id"].as_str().unwrap();

    // The student sits down — that's the live-attendance signal.
    let res = student
        .post(format!("{base}/exams/{exam_id}/attempt"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // Open the stream and read until one full snapshot event has arrived.
    let mut res = teacher
        .get(format!("{base}/exams/{exam_id}/live/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
        .headers()
        .get("content-type")
        .expect("content-type")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    let mut body = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !(body.contains("event: snapshot") && body.contains("\n\n")) {
        let chunk = tokio::time::timeout_at(deadline, res.chunk())
            .await
            .expect("a snapshot event within 5s")
            .expect("stream stays open")
            .expect("stream yields data");
        body.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(body.contains("\"in_progress\""), "{body}");
    assert!(body.contains("veli"), "{body}");
    assert!(body.contains("\"enrolled\":1"), "{body}");
}

/// Read the open SSE stream `res` until a `snapshot` event whose parsed JSON
/// satisfies `want`, and return that snapshot. `buf` carries bytes between
/// calls, so a partial event at the end of one read completes in the next and
/// events already buffered are drained before more are pulled. Panics if
/// nothing matching arrives within `within`.
async fn next_matching_snapshot(
    res: &mut reqwest::Response,
    buf: &mut String,
    within: std::time::Duration,
    want: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        // Drain every complete event (`\n\n`-terminated) already buffered
        // before pulling more bytes — keep-alive comments and non-snapshot
        // frames are skipped, non-matching snapshots consumed and dropped.
        while let Some(end) = buf.find("\n\n") {
            let event: String = buf.drain(..end + 2).collect();
            if !event.contains("event: snapshot") {
                continue;
            }
            let data = event
                .lines()
                .find_map(|line| line.strip_prefix("data:"))
                .expect("a snapshot event carries a data line")
                .trim();
            let snapshot: Value = serde_json::from_str(data).expect("snapshot is json");
            if want(&snapshot) {
                return snapshot;
            }
        }
        let chunk = tokio::time::timeout_at(deadline, res.chunk())
            .await
            .expect("an SSE event before the deadline")
            .expect("stream stays open")
            .expect("stream yields data");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// The teacher's monitor updates *live* on one held-open connection: after the
/// first snapshot lands, a student's submission must surface in a *later* event
/// on the *same* stream, no reconnect. Guards against a monitor that replays a
/// cached first snapshot or stops re-reading the exam each tick — the split
/// coverage (one-shot `/live` correctness + a single streamed event) would miss
/// that.
#[tokio::test]
async fn live_stream_reflects_a_status_change_on_the_open_connection() {
    let (base, db) = spawn_server().await;
    let teacher = client();
    register(&teacher, &base, "hoca").await;
    promote(&db, "hoca", "teacher").await;
    login(&teacher, &base, "hoca").await;

    let student = client();
    register(&student, &base, "veli").await;
    login(&student, &base, "veli").await;
    let student_id: Value = student
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let student_id = student_id["id"].as_str().unwrap().to_string();

    let now: Value = teacher
        .get(format!("{base}/time"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let now = now["now"].as_i64().unwrap();

    let course: Value = teacher
        .post(format!("{base}/courses"))
        .json(&json!({ "title": "algebra" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let course_id = course["id"].as_str().unwrap();
    let res = teacher
        .post(format!("{base}/courses/{course_id}/enrollments"))
        .json(&json!({ "user_id": student_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let exam: Value = teacher
        .post(format!("{base}/courses/{course_id}/exams"))
        .json(&json!({
            "title": "final", "kind": "final",
            "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exam_id = exam["id"].as_str().unwrap();

    // The student sits down — in progress, nothing submitted.
    let res = student
        .post(format!("{base}/exams/{exam_id}/attempt"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // The teacher opens the live stream and reads the first snapshot.
    let mut res = teacher
        .get(format!("{base}/exams/{exam_id}/live/stream"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()
            .get("content-type")
            .expect("content-type")
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let mut buf = String::new();
    let first =
        next_matching_snapshot(&mut res, &mut buf, std::time::Duration::from_secs(5), |s| {
            s["counts"]["in_progress"].as_i64() == Some(1)
        })
        .await;
    assert_eq!(first["counts"]["submitted"], 0, "{first}");
    assert_eq!(first["students"][0]["user"]["username"], "veli");
    assert_eq!(first["students"][0]["status"], "in_progress", "{first}");

    // The student submits while the teacher's stream stays open.
    let res_finish = student
        .post(format!("{base}/exams/{exam_id}/attempt/finish"))
        .send()
        .await
        .unwrap();
    assert_eq!(res_finish.status(), StatusCode::OK);

    // A later event on the same connection must carry the change — the monitor
    // re-reads and recomputes each tick, it does not replay the first snapshot.
    let later =
        next_matching_snapshot(&mut res, &mut buf, std::time::Duration::from_secs(8), |s| {
            s["counts"]["submitted"].as_i64() == Some(1)
        })
        .await;
    assert_eq!(later["counts"]["in_progress"], 0, "{later}");
    assert_eq!(later["students"][0]["status"], "submitted", "{later}");
}

/// Log in without a cookie jar and hand back the raw `session=<token>` pair —
/// the exact header value the WebSocket handshake needs (tungstenite carries
/// no jar of its own).
async fn raw_session_cookie(base: &str, user: &str) -> String {
    let res = Client::new()
        .post(format!("{base}/auth/login"))
        .json(&json!({ "username": user, "password": "secret1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "login {user}");
    res.headers()
        .get("set-cookie")
        .expect("session cookie set on login")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

// ---- the exam room (WebSocket) ---------------------------------------------

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the exam room, optionally authenticated. `Ok` is the upgraded socket;
/// `Err` is the HTTP status a pre-upgrade gate refused with.
async fn ws_open(base: &str, exam_id: &str, cookie: Option<&str>) -> Result<WsStream, u16> {
    let url = format!(
        "{}/exams/{exam_id}/attempt/ws",
        base.replace("http://", "ws://")
    );
    let mut request = url.into_client_request().unwrap();
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
    }
    match connect_async(request).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            Err(response.status().as_u16())
        }
        Err(other) => panic!("unexpected handshake failure: {other}"),
    }
}

async fn ws_send(ws: &mut WsStream, frame: Value) {
    ws.send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}

/// The next JSON text frame, within a deadline; `None` once the server closes.
/// 6 s covers the room's 2 s tick with room to spare on a loaded machine.
async fn ws_next_frame(ws: &mut WsStream) -> Option<Value> {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(6), ws.next())
            .await
            .expect("a frame within 6s")?
            .expect("stream stays healthy");
        match message {
            Message::Text(text) => {
                return Some(serde_json::from_str(text.as_str()).expect("JSON frame"));
            }
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

/// Skip periodic `state` ticks until a frame of `kind` arrives.
async fn ws_frame_of_type(ws: &mut WsStream, kind: &str) -> Value {
    loop {
        let frame = ws_next_frame(ws)
            .await
            .unwrap_or_else(|| panic!("room closed while waiting for a {kind:?} frame"));
        if frame["type"] == kind {
            return frame;
        }
        assert_eq!(frame["type"], "state", "unexpected frame: {frame}");
    }
}

/// A booted server with teacher `hoca`, enrolled student `veli` (jar client +
/// raw cookie for the handshake), and an open sync exam holding one choice
/// question — the spine of every exam-room test. The window closes
/// `window_ms` from now.
struct ExamRoom {
    base: String,
    db: Database,
    teacher: Client,
    student: Client,
    student_id: String,
    cookie: String,
    course_id: String,
    subject_id: String,
    exam_id: String,
    question_id: String,
}

async fn exam_room_fixture(window_ms: i64) -> ExamRoom {
    let (base, db) = spawn_server().await;
    let teacher = client();
    register(&teacher, &base, "hoca").await;
    promote(&db, "hoca", "teacher").await;
    login(&teacher, &base, "hoca").await;

    let student = client();
    register(&student, &base, "veli").await;
    login(&student, &base, "veli").await;
    let me: Value = student
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let student_id = me["id"].as_str().unwrap().to_string();

    let now: Value = teacher
        .get(format!("{base}/time"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let now = now["now"].as_i64().unwrap();
    let course: Value = teacher
        .post(format!("{base}/courses"))
        .json(&json!({ "title": "algebra" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let course_id = course["id"].as_str().unwrap().to_string();
    let subject: Value = teacher
        .post(format!("{base}/courses/{course_id}/subjects"))
        .json(&json!({ "name": "arithmetic" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let subject_id = subject["id"].as_str().unwrap().to_string();
    let res = teacher
        .post(format!("{base}/courses/{course_id}/enrollments"))
        .json(&json!({ "user_id": student_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let exam: Value = teacher
        .post(format!("{base}/courses/{course_id}/exams"))
        .json(&json!({
            "title": "final", "kind": "final",
            "mode": "sync", "starts_at": now - 1_000, "ends_at": now + window_ms,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exam_id = exam["id"].as_str().unwrap().to_string();
    let question: Value = teacher
        .post(format!("{base}/exams/{exam_id}/questions"))
        .json(
            &json!({ "subject_id": subject_id, "text": "2 + 2?", "kind": "choice",
                       "points": 10, "choices": ["3", "4"], "correct": 1 }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();
    let cookie = raw_session_cookie(&base, "veli").await;
    ExamRoom {
        base,
        db,
        teacher,
        student,
        student_id,
        cookie,
        course_id,
        subject_id,
        exam_id,
        question_id,
    }
}

/// The student exam room over a real TCP WebSocket: connect with the session
/// cookie, get the state frame, autosave an answer, survive junk input,
/// finish, and watch the server close the room — with the result visible to
/// the teacher over REST.
#[tokio::test]
async fn exam_room_websocket_round_trip() {
    let room = exam_room_fixture(600_000).await;
    let ExamRoom {
        base,
        teacher,
        student,
        student_id,
        cookie,
        exam_id,
        question_id,
        ..
    } = &room;
    // A second question so progress counts have something to be partial over.
    let res = teacher
        .post(format!("{base}/exams/{exam_id}/questions"))
        .json(&json!({ "subject_id": room.subject_id, "text": "Explain.",
                       "kind": "text", "points": 20 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // The room requires an attempt — the gate rejects at HTTP time (404),
    // before any upgrade.
    assert_eq!(
        ws_open(base, exam_id, Some(cookie)).await.err(),
        Some(404),
        "start the attempt first"
    );

    let res = student
        .post(format!("{base}/exams/{exam_id}/attempt"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    let mut ws = ws_open(base, exam_id, Some(cookie)).await.expect("upgrade");

    // Connect-time state: in progress, nothing answered yet, server clock in.
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    assert_eq!(state["type"], "state", "{state}");
    assert_eq!(state["status"], "in_progress");
    assert_eq!(state["answered"], 0);
    assert_eq!(state["question_count"], 2);
    assert!(state["remaining_ms"].as_i64().unwrap() > 0);
    assert!(state["now"].as_i64().is_some());

    // Ping/pong keeps the client's clock honest between ticks.
    ws_send(&mut ws, json!({ "type": "ping" })).await;
    ws_frame_of_type(&mut ws, "pong").await;

    // Autosave: answer -> saved ack -> a state showing the progress.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": 1 }),
    )
    .await;
    let saved = ws_frame_of_type(&mut ws, "saved").await;
    assert_eq!(saved["question_id"], question_id.as_str());
    assert!(saved["updated_at"].as_i64().is_some());
    let state = ws_frame_of_type(&mut ws, "state").await;
    assert_eq!(state["answered"], 1, "{state}");

    // A payload that doesn't fit the question is an error frame, not a close.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "text": "4" }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("selected"),
        "{error}"
    );

    // Junk and unknown message types too — the room shrugs and stays up.
    ws.send(Message::Text("not json".into())).await.unwrap();
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("unrecognized"),
        "{error}"
    );
    ws_send(&mut ws, json!({ "type": "selfdestruct" })).await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("unrecognized"),
        "{error}"
    );
    ws_send(&mut ws, json!({ "type": "ping" })).await;
    ws_frame_of_type(&mut ws, "pong").await;

    // Finish: acknowledged, then the server closes the room.
    ws_send(&mut ws, json!({ "type": "finish" })).await;
    let finished = ws_frame_of_type(&mut ws, "finished").await;
    assert!(finished["finished_at"].as_i64().is_some(), "{finished}");
    loop {
        match ws_next_frame(&mut ws).await {
            None => break, // closed — as promised
            Some(frame) => assert_eq!(frame["type"], "state", "{frame}"),
        }
    }

    // The submission and the saved answer are visible over REST.
    let attempt: Value = student
        .get(format!("{base}/exams/{exam_id}/attempt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attempt["status"], "submitted");
    assert_eq!(attempt["answered"], 1);

    let sheet: Value = teacher
        .get(format!(
            "{base}/exams/{exam_id}/attempts/{student_id}/answers"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sheet["answers"][0]["is_correct"], true);
    assert_eq!(sheet["auto_score"]["earned"], 10);
    assert_eq!(sheet["auto_score"]["possible"], 10);
}

/// Every pre-upgrade gate answers with a proper HTTP status, so a rejected
/// client sees *why* instead of an instant close.
#[tokio::test]
async fn exam_room_rejects_bad_handshakes() {
    let room = exam_room_fixture(600_000).await;

    // No session cookie: 401 before anything else.
    assert_eq!(
        ws_open(&room.base, &room.exam_id, None).await.err(),
        Some(401)
    );

    // Unknown exam: 404.
    assert_eq!(
        ws_open(&room.base, "does-not-exist", Some(&room.cookie))
            .await
            .err(),
        Some(404)
    );

    // An unscheduled exam has no room to join: 409.
    let unscheduled: Value = room
        .teacher
        .post(format!("{}/courses/{}/exams", room.base, room.course_id))
        .json(&json!({ "title": "homework", "kind": "homework" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let unscheduled_id = unscheduled["id"].as_str().unwrap();
    assert_eq!(
        ws_open(&room.base, unscheduled_id, Some(&room.cookie))
            .await
            .err(),
        Some(409)
    );

    // Not enrolled: 403, even on a scheduled, open exam.
    let outsider = client();
    register(&outsider, &room.base, "omer").await;
    let outsider_cookie = raw_session_cookie(&room.base, "omer").await;
    assert_eq!(
        ws_open(&room.base, &room.exam_id, Some(&outsider_cookie))
            .await
            .err(),
        Some(403)
    );

    // A submitted attempt has nothing left to write: 409.
    let res = room
        .student
        .post(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/finish",
            room.base, room.exam_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        ws_open(&room.base, &room.exam_id, Some(&room.cookie))
            .await
            .err(),
        Some(409)
    );
}

/// A promotion out of `student` mid-exam closes the open room's answer sheet
/// on the very next save: the role wall is re-judged per save against the
/// live row — like the enrollment wall — so the door check is never the last
/// word for a socket that outlives the role. A demotion back reopens the
/// sheet on the same socket: it is a live check, not a latch.
#[tokio::test]
async fn exam_room_promotion_mid_exam_closes_the_sheet() {
    let room = exam_room_fixture(600_000).await;
    let ExamRoom {
        base,
        db,
        teacher,
        student,
        student_id,
        cookie,
        exam_id,
        question_id,
        ..
    } = &room;
    let res = student
        .post(format!("{base}/exams/{exam_id}/attempt"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let mut ws = ws_open(base, exam_id, Some(cookie)).await.expect("upgrade");

    // While the sitter is a student the sheet is open — save one answer.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": 1 }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;

    // Mid-exam, with the socket still open, the sitter stops being a student.
    promote(db, "veli", "teacher").await;

    // The very next save is refused over the same socket...
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": 0 }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("only students"),
        "{error}"
    );

    // ... and over REST — the two paths share the wall.
    let res = student
        .post(format!("{base}/exams/{exam_id}/attempt/answers"))
        .json(&json!({ "question_id": question_id, "selected": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // The refused writes left no trace: the sheet still holds the original.
    let sheet: Value = teacher
        .get(format!(
            "{base}/exams/{exam_id}/attempts/{student_id}/answers"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sheet["answers"].as_array().unwrap().len(), 1);
    assert_eq!(sheet["answers"][0]["selected"], 1);

    // Demoted back, the same socket writes again — no reconnect required.
    promote(db, "veli", "student").await;
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": 0 }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;
}

/// A teacher extending `ends_at` mid-exam moves the open room's countdown on
/// the next tick — the deadline is re-read, never cached in the socket.
#[tokio::test]
async fn exam_room_deadline_moves_with_a_live_extension() {
    let room = exam_room_fixture(600_000).await;
    let res = room
        .student
        .post(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    let mut ws = ws_open(&room.base, &room.exam_id, Some(&room.cookie))
        .await
        .expect("upgrade");
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    let deadline = state["deadline"].as_i64().expect("deadline");

    let extended = deadline + 300_000;
    let res = room
        .teacher
        .patch(format!("{}/exams/{}", room.base, room.exam_id))
        .json(&json!({ "ends_at": extended }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Within a couple of ticks the room follows.
    let mut moved = false;
    for _ in 0..5 {
        let state = ws_frame_of_type(&mut ws, "state").await;
        if state["deadline"].as_i64() == Some(extended) {
            assert!(state["remaining_ms"].as_i64().unwrap() > 300_000);
            moved = true;
            break;
        }
    }
    assert!(moved, "the extension reaches the open room");
}

/// When the deadline passes mid-session, a tick notices: the room announces
/// `expired`, closes, and everything saved in time survives for grading.
#[tokio::test]
async fn exam_room_expires_mid_session() {
    // The window closes ~2.6 s in — enough to connect and save, gone by the
    // second tick. Real time: expiry is judged by the server clock.
    let room = exam_room_fixture(2_600).await;
    let res = room
        .student
        .post(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    let mut ws = ws_open(&room.base, &room.exam_id, Some(&room.cookie))
        .await
        .expect("upgrade");
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    assert_eq!(state["status"], "in_progress", "{state}");

    // One answer lands inside the window.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": room.question_id, "selected": 1 }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;

    // A tick notices the deadline: `expired`, then the room closes.
    ws_frame_of_type(&mut ws, "expired").await;
    assert!(
        ws_next_frame(&mut ws).await.is_none(),
        "room closes after expiring"
    );

    // The server agrees over REST: no more writes, the attempt is expired,
    // and the in-time answer is still there for the grader.
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/answers",
            room.base, room.exam_id
        ))
        .json(&json!({ "question_id": room.question_id, "selected": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let attempt: Value = room
        .student
        .get(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attempt["status"], "expired");
    assert_eq!(attempt["answered"], 1);
    let sheet: Value = room
        .teacher
        .get(format!(
            "{}/exams/{}/attempts/{}/answers",
            room.base, room.exam_id, room.student_id
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sheet["answers"][0]["selected"], 1);
    assert_eq!(sheet["auto_score"]["earned"], 10);
}

/// The student's latest attempt as their own REST view sees it.
async fn my_attempt(room: &ExamRoom) -> Value {
    room.student
        .get(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Wait (bounded) for the room teardown to stamp `left_at` — the socket close
/// and the server's stamp race, so poll the REST view briefly.
async fn wait_for_left_at(room: &ExamRoom) -> i64 {
    for _ in 0..50 {
        let attempt = my_attempt(room).await;
        if let Some(left_at) = attempt["left_at"].as_i64() {
            return left_at;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("left_at was never stamped after the socket closed");
}

/// The rejoin door: with `allow_rejoin` off, walking out of the room stamps
/// `left_at` and locks re-entry and further saves (REST included) — finish
/// stays possible — until the teacher flips the door back open, which lets
/// the student reconnect (clearing `left_at`) and keep answering.
#[tokio::test]
async fn exam_room_rejoin_door_is_the_teachers_call() {
    let room = exam_room_fixture(600_000).await;

    // Close the door before anyone sits.
    let res = room
        .teacher
        .patch(format!("{}/exams/{}", room.base, room.exam_id))
        .json(&json!({ "allow_rejoin": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = room
        .student
        .post(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // First entry is fine — the door only matters after a walk-out.
    let mut ws = ws_open(&room.base, &room.exam_id, Some(&room.cookie))
        .await
        .expect("first entry");
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    assert_eq!(state["status"], "in_progress", "{state}");
    assert_eq!(state["attempt"], 1, "{state}");
    ws.close(None).await.unwrap();
    let left_at = wait_for_left_at(&room).await;
    assert!(left_at > 0);

    // Locked out: no REST saves, no re-entry.
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/answers",
            room.base, room.exam_id
        ))
        .json(&json!({ "question_id": room.question_id, "selected": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    assert_eq!(
        ws_open(&room.base, &room.exam_id, Some(&room.cookie))
            .await
            .expect_err("rejoin is closed"),
        409
    );

    // The teacher reopens the door live; the student walks back in, which
    // clears the stamp, and answering works again.
    let res = room
        .teacher
        .patch(format!("{}/exams/{}", room.base, room.exam_id))
        .json(&json!({ "allow_rejoin": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut ws = ws_open(&room.base, &room.exam_id, Some(&room.cookie))
        .await
        .expect("rejoin after the teacher reopened");
    ws_next_frame(&mut ws).await.expect("state after rejoin");
    let attempt = my_attempt(&room).await;
    assert!(
        attempt["left_at"].is_null(),
        "re-entry clears left_at: {attempt}"
    );
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": room.question_id, "selected": 1 }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;

    // Walking out again with the door open: stamped, but REST saves still
    // land — rejoin is allowed, the stamp is just presence.
    ws.close(None).await.unwrap();
    wait_for_left_at(&room).await;
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/answers",
            room.base, room.exam_id
        ))
        .json(&json!({ "question_id": room.question_id, "selected": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // A locked-out student can still submit what they saved: close the door
    // once more and finish over REST.
    let res = room
        .teacher
        .patch(format!("{}/exams/{}", room.base, room.exam_id))
        .json(&json!({ "allow_rejoin": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/finish",
            room.base, room.exam_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "finish is exempt from rejoin");
}

/// A walk-out stamp belongs to the sitting the room was opened for — never to
/// a retake started while the old socket lingered. Closing the stale room of a
/// finished sitting must not mark the *new* sitting as left (with the door
/// closed, that stamp would lock the student out of an exam room they never
/// entered).
#[tokio::test]
async fn exam_room_close_after_a_retake_leaves_the_new_sitting_alone() {
    let room = exam_room_fixture(600_000).await;

    // An open exam with two sittings and the rejoin door closed.
    let exam: Value = room
        .teacher
        .post(format!("{}/courses/{}/exams", room.base, room.course_id))
        .json(&json!({
            "title": "practice", "kind": "quiz",
            "mode": "open", "max_attempts": 2, "allow_rejoin": false,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exam_id = exam["id"].as_str().unwrap().to_string();
    let question: Value = room
        .teacher
        .post(format!("{}/exams/{exam_id}/questions", room.base))
        .json(
            &json!({ "subject_id": room.subject_id, "text": "3 + 3?", "kind": "choice",
                       "points": 5, "choices": ["5", "6"], "correct": 1 }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();

    // Sit sitting #1 and open its room.
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let mut ws = ws_open(&room.base, &exam_id, Some(&room.cookie))
        .await
        .expect("room for sitting #1");
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    assert_eq!(state["attempt"], 1, "{state}");

    // Finish sitting #1 and start sitting #2 over REST — the old socket is
    // still open while the retake begins.
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt/finish", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["attempt"], 2);

    // Now walk out of the stale sitting-#1 room and give the teardown time.
    ws.close(None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Sitting #2 was never entered, so it was never left: no stamp, and
    // answering it works despite the closed door.
    let attempt: Value = room
        .student
        .get(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attempt["attempt"], 2, "{attempt}");
    assert!(
        attempt["left_at"].is_null(),
        "closing the old sitting's room must not stamp the new sitting: {attempt}"
    );
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt/answers", room.base))
        .json(&json!({ "question_id": question_id, "selected": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "sitting #2 is writable — the student never left it"
    );
}

/// Messages through a stale room act on that room's own sitting — never on a
/// retake started while the old socket lingered. An `answer` or `finish` sent
/// through the old room after sitting #1 ended must be refused, not silently
/// applied to sitting #2 (which would scribble on — or instantly submit — a
/// fresh attempt the student opened elsewhere, burning a limited sitting).
#[tokio::test]
async fn exam_room_messages_bind_to_their_own_sitting() {
    let room = exam_room_fixture(600_000).await;

    // An open exam with two sittings and one choice question.
    let exam: Value = room
        .teacher
        .post(format!("{}/courses/{}/exams", room.base, room.course_id))
        .json(&json!({
            "title": "practice", "kind": "quiz",
            "mode": "open", "max_attempts": 2,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exam_id = exam["id"].as_str().unwrap().to_string();
    let question: Value = room
        .teacher
        .post(format!("{}/exams/{exam_id}/questions", room.base))
        .json(
            &json!({ "subject_id": room.subject_id, "text": "3 + 3?", "kind": "choice",
                       "points": 5, "choices": ["5", "6"], "correct": 1 }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();

    // Sit sitting #1 and open its room.
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let mut ws = ws_open(&room.base, &exam_id, Some(&room.cookie))
        .await
        .expect("room for sitting #1");
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    assert_eq!(state["attempt"], 1, "{state}");

    // Finish sitting #1 and start sitting #2 over REST — the old socket is
    // still open while the retake begins.
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt/finish", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["attempt"], 2);

    // An answer through the stale room must be refused — not saved into the
    // blank sheet of sitting #2.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": 1 }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("sitting"),
        "{error}"
    );

    // A finish through the stale room must be refused — not submit sitting #2.
    ws_send(&mut ws, json!({ "type": "finish" })).await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("sitting"),
        "{error}"
    );

    // Sitting #2 is untouched: still running, sheet still blank.
    let attempt: Value = room
        .student
        .get(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(attempt["attempt"], 2, "{attempt}");
    assert_eq!(
        attempt["status"], "in_progress",
        "a stale room must not submit the new sitting: {attempt}"
    );
    assert_eq!(
        attempt["answered"], 0,
        "a stale room must not write into the new sitting's sheet: {attempt}"
    );
}

/// Two sockets on the same sitting (a second tab): closing one must not count
/// as leaving the exam room while the other is still connected — only the last
/// socket out stamps `left_at` and (with the door closed) locks answering.
#[tokio::test]
async fn exam_room_second_tab_keeps_the_student_present() {
    let room = exam_room_fixture(600_000).await;

    // Close the door before anyone sits, so a false stamp locks immediately.
    let res = room
        .teacher
        .patch(format!("{}/exams/{}", room.base, room.exam_id))
        .json(&json!({ "allow_rejoin": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = room
        .student
        .post(format!("{}/exams/{}/attempt", room.base, room.exam_id))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // Two tabs in the same room.
    let mut ws1 = ws_open(&room.base, &room.exam_id, Some(&room.cookie))
        .await
        .expect("first tab");
    ws_next_frame(&mut ws1).await.expect("state on tab one");
    let mut ws2 = ws_open(&room.base, &room.exam_id, Some(&room.cookie))
        .await
        .expect("second tab");
    ws_next_frame(&mut ws2).await.expect("state on tab two");

    // Closing one tab is not leaving: the student is still in the room.
    ws1.close(None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let attempt = my_attempt(&room).await;
    assert!(
        attempt["left_at"].is_null(),
        "one closed tab of two must not stamp a walk-out: {attempt}"
    );
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/answers",
            room.base, room.exam_id
        ))
        .json(&json!({ "question_id": room.question_id, "selected": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "still present via the second tab, so saves land"
    );

    // The last tab closing is the real walk-out: stamped, and the closed door
    // now locks further saves.
    ws2.close(None).await.unwrap();
    wait_for_left_at(&room).await;
    let res = room
        .student
        .post(format!(
            "{}/exams/{}/attempt/answers",
            room.base, room.exam_id
        ))
        .json(&json!({ "question_id": room.question_id, "selected": 0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
}

/// An `open`-mode exam room: no deadline in the state frames, and a retake
/// (attempt #2) enters the room with a blank sheet after the first sitting is
/// submitted.
#[tokio::test]
async fn exam_room_open_mode_runs_untimed_and_retakes() {
    let room = exam_room_fixture(600_000).await;

    // A second, open exam in the same course: two sittings, no window.
    let exam: Value = room
        .teacher
        .post(format!("{}/courses/{}/exams", room.base, room.course_id))
        .json(&json!({
            "title": "practice", "kind": "quiz",
            "mode": "open", "max_attempts": 2,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let exam_id = exam["id"].as_str().unwrap().to_string();
    let question: Value = room
        .teacher
        .post(format!("{}/exams/{exam_id}/questions", room.base))
        .json(
            &json!({ "subject_id": room.subject_id, "text": "3 + 3?", "kind": "choice",
                       "points": 5, "choices": ["5", "6"], "correct": 1 }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();

    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    let mut ws = ws_open(&room.base, &exam_id, Some(&room.cookie))
        .await
        .expect("open-mode room");
    let state = ws_next_frame(&mut ws).await.expect("connect state");
    assert_eq!(state["status"], "in_progress", "{state}");
    assert_eq!(state["attempt"], 1, "{state}");
    assert!(state["deadline"].is_null(), "{state}");
    assert!(state["remaining_ms"].is_null(), "{state}");

    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": 1 }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;
    ws_send(&mut ws, json!({ "type": "finish" })).await;
    ws_frame_of_type(&mut ws, "finished").await;
    assert!(ws_next_frame(&mut ws).await.is_none(), "room closes");

    // Retake: sitting #2, blank sheet, fresh room.
    let res = room
        .student
        .post(format!("{}/exams/{exam_id}/attempt", room.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["attempt"], 2);
    assert_eq!(body["answered"], 0);

    let mut ws = ws_open(&room.base, &exam_id, Some(&room.cookie))
        .await
        .expect("room for the retake");
    let state = ws_next_frame(&mut ws).await.expect("state");
    assert_eq!(state["attempt"], 2, "{state}");
    assert_eq!(state["answered"], 0, "{state}");
    ws.close(None).await.unwrap();
}
