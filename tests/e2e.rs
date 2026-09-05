//! End-to-end tests: boot the real server on an ephemeral TCP port and drive it
//! with `reqwest` over HTTP, using its cookie jar exactly like a browser client.

use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::tenant::{DEMO_SLUG, Slug};
use hezarfen_backend::{build_router, database};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

/// Start the server on a random port. Returns its base URL (e.g.
/// `http://127.0.0.1:54321`) plus a handle to its database, so a test can grant
/// roles the same out-of-band way production does.
async fn spawn_server() -> (String, Database) {
    spawn_server_with_ai(None).await
}

/// [`spawn_server`], with the AI bridge the chatbot relays through wired in.
async fn spawn_server_with_ai(ai: Option<hezarfen_backend::ai::AiBridge>) -> (String, Database) {
    let tenants = database::init_mem_tenants()
        .await
        .expect("in-memory deployment");
    let db = tenants
        .get(&Slug::try_new(DEMO_SLUG).unwrap())
        .await
        .expect("the demo school");
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        // Kept (not auto-deleted) so the directory outlives this helper;
        // it's under the OS temp dir, reclaimed like any other temp file.
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        // Every request here comes from 127.0.0.1, so per-IP limits would
        // meter the whole suite as one client. Off; `rate_limit.rs` covers it.
        rate_limit: RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai,
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
        .json(&json!({ "school": "demo", "username": user, "password": "secret1" }))
        .send()
        .await
        .unwrap()
}

async fn login(client: &Client, base: &str, user: &str) {
    let res = client
        .post(format!("{base}/auth/login"))
        .json(&json!({ "school": "demo", "username": user, "password": "secret1" }))
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

    // A student may search since #24 — and sees staff only: ali the teacher
    // comes back, another student never would.
    let found: Value = veli
        .get(format!("{base}/users/search?q=ali"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
    assert_eq!(found["items"][0]["username"], "ali");
    let res = veli
        .get(format!("{base}/users/search?q=vel&role=student"))
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

/// Log in without a cookie jar and hand back the raw `session=<token>` pair —
/// the exact header value the WebSocket handshake needs (tungstenite carries
/// no jar of its own).
async fn raw_session_cookie(base: &str, user: &str) -> String {
    let res = Client::new()
        .post(format!("{base}/auth/login"))
        .json(&json!({ "school": "demo", "username": user, "password": "secret1" }))
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
    /// The fixture question's minted choice ids, in list order — what the
    /// tests below used to write as the indexes 0 and 1.
    choice_ids: Vec<String>,
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
                       "points": 10, "choices": [{"id": "a", "text": "3"}, {"id": "b", "text": "4"}], "correct": "b" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();
    let choice_ids = choice_ids(&question);
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
        choice_ids,
    }
}

/// The minted ids of a question response's options, in list order.
fn choice_ids(question: &Value) -> Vec<String> {
    question["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice["id"].as_str().unwrap().to_string())
        .collect()
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
        json!({ "type": "answer", "question_id": question_id, "selected": room.choice_ids[1] }),
    )
    .await;
    let saved = ws_frame_of_type(&mut ws, "saved").await;
    assert_eq!(saved["question_id"], question_id.as_str());
    assert!(saved["updated_at"].as_i64().is_some());
    // No `client_seq` was sent, so no `client_seq` comes back — not even a null. Clients
    // that predate the field see exactly the frames they always saw.
    assert!(saved.get("client_seq").is_none(), "{saved}");
    let state = ws_frame_of_type(&mut ws, "state").await;
    assert_eq!(state["answered"], 1, "{state}");

    // With a `client_seq`, the ack carries it back verbatim: `question_id` alone
    // cannot settle a send, since a re-save after a timeout leaves two of them
    // outstanding for the same question. The server assigns it no meaning —
    // a repeat of an already-used value is saved and echoed like any other.
    for _ in 0..2 {
        ws_send(
            &mut ws,
            json!({ "type": "answer", "question_id": question_id,
                    "selected": room.choice_ids[1], "client_seq": 7 }),
        )
        .await;
        let saved = ws_frame_of_type(&mut ws, "saved").await;
        assert_eq!(saved["client_seq"], 7, "{saved}");
        assert_eq!(saved["question_id"], question_id.as_str());
        ws_frame_of_type(&mut ws, "state").await;
    }

    // A failed save echoes it too — that is the whole point: the client can
    // fail the exact send, even when the error names no question.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": "x".repeat(70_000), "text": "x", "client_seq": 8 }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert_eq!(error["client_seq"], 8, "{error}");
    assert!(error.get("question_id").is_none(), "{error}");
    // ... and `client_seq` rides alongside the blame when there is one.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "text": "4", "client_seq": 9 }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert_eq!(error["client_seq"], 9, "{error}");
    assert_eq!(error["question_id"], question_id.as_str(), "{error}");

    // `client_seq` is legal only because it is a declared field: everything else on
    // an `answer` is still refused outright.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id,
                "selected": room.choice_ids[1], "sequence": 10 }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("unrecognized"),
        "{error}"
    );
    assert!(error.get("client_seq").is_none(), "{error}");

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
    // ... and it names the question it belongs to, so a client with several
    // saves in flight fails only this one.
    assert_eq!(error["question_id"], question_id.as_str(), "{error}");

    // A question that isn't in this exam is attributable too — that one save
    // is doomed, the others aren't.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": "nosuchquestion", "text": "x" }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert_eq!(error["question_id"], "nosuchquestion", "{error}");

    // An over-long `question_id` dies at the door and is *not* reflected. The
    // error frame echoes the field it blames, and axum accepts frames up to
    // 64 MiB, so an uncapped echo is a self-inflicted amplifier — uncapped,
    // this arrives back with all 70 000 characters attached.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": "x".repeat(70_000), "text": "x" }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("question_id"), "{error}");
    assert!(message.len() < 200, "the id itself is never quoted back");
    assert!(error.get("question_id").is_none(), "{error}");

    // Junk and unknown message types too — the room shrugs and stays up.
    // These are frame-level, not save-level: no question to blame, so the
    // client keeps failing everything in flight.
    ws.send(Message::Text("not json".into())).await.unwrap();
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("unrecognized"),
        "{error}"
    );
    assert!(error.get("question_id").is_none(), "{error}");
    ws_send(&mut ws, json!({ "type": "selfdestruct" })).await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("unrecognized"),
        "{error}"
    );
    assert!(error.get("question_id").is_none(), "{error}");
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
        json!({ "type": "answer", "question_id": question_id, "selected": room.choice_ids[1] }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;

    // Mid-exam, with the socket still open, the sitter stops being a student.
    promote(db, "veli", "teacher").await;

    // The very next save is refused over the same socket...
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": room.choice_ids[0] }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("only students"),
        "{error}"
    );
    // Sitter-level, not question-level: the sheet is closed for every question,
    // so the frame blames none of them and the client fails everything.
    assert!(error.get("question_id").is_none(), "{error}");

    // ... and over REST — the two paths share the wall.
    let res = student
        .post(format!("{base}/exams/{exam_id}/attempt/answers"))
        .json(&json!({ "question_id": question_id, "selected": room.choice_ids[0] }))
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
    assert_eq!(sheet["answers"][0]["selected"], room.choice_ids[1]);

    // Demoted back, the same socket writes again — no reconnect required.
    promote(db, "veli", "student").await;
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": question_id, "selected": room.choice_ids[0] }),
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
    // A generous window while the room is set up. Keying the deadline off
    // *fixture* time (this used to ask for 2.6 s) made the whole fixture race
    // it — five HTTP calls plus a `raw_session_cookie` login, and argon2 is
    // deliberately expensive, so on a loaded run the setup alone outran the
    // window and the attempt POST below hit an already-closed exam (409).
    // Nothing here cares *when* the window closes, only that it closes with the
    // room open — so close it deliberately, further down, once it is.
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
    assert_eq!(state["status"], "in_progress", "{state}");

    // One answer lands inside the window.
    ws_send(
        &mut ws,
        json!({ "type": "answer", "question_id": room.question_id, "selected": room.choice_ids[1] }),
    )
    .await;
    ws_frame_of_type(&mut ws, "saved").await;

    // Now bring the deadline in, from a *fresh* server clock — the room re-reads
    // the schedule every tick, the same live-schedule path
    // `exam_room_deadline_moves_with_a_live_extension` covers, in the other
    // direction. Setup latency is behind us, so the only wait left is one tick.
    let now: Value = room
        .teacher
        .get(format!("{}/time", room.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let res = room
        .teacher
        .patch(format!("{}/exams/{}", room.base, room.exam_id))
        .json(&json!({ "ends_at": now["now"].as_i64().unwrap() + 1_000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

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
        .json(&json!({ "question_id": room.question_id, "selected": room.choice_ids[0] }))
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
    assert_eq!(sheet["answers"][0]["selected"], room.choice_ids[1]);
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
        .json(&json!({ "question_id": room.question_id, "selected": room.choice_ids[1] }))
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
        json!({ "type": "answer", "question_id": room.question_id, "selected": room.choice_ids[1] }),
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
        .json(&json!({ "question_id": room.question_id, "selected": room.choice_ids[0] }))
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
                       "points": 5, "choices": [{"id": "a", "text": "5"}, {"id": "b", "text": "6"}], "correct": "b" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();
    let opts = choice_ids(&question);

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
    // race-window staging — do not convert to poll: sitting #1 is terminal, so
    // the teardown stamps nothing at all (`stamp_left` writes only an
    // in-progress sitting), and the assertion below is that *no* stamp lands on
    // sitting #2 — a negative with no positive to wait on.
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
        .json(&json!({ "question_id": question_id, "selected": opts[1] }))
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
                       "points": 5, "choices": [{"id": "a", "text": "5"}, {"id": "b", "text": "6"}], "correct": "b" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();
    let opts = choice_ids(&question);

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
        json!({ "type": "answer", "question_id": question_id, "selected": opts[1] }),
    )
    .await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("sitting"),
        "{error}"
    );
    // The room's sitting is over — that's not this question's fault, so no
    // blame is attached and the client fails every pending save.
    assert!(error.get("question_id").is_none(), "{error}");

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
        .json(&json!({ "question_id": room.question_id, "selected": room.choice_ids[1] }))
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
        .json(&json!({ "question_id": room.question_id, "selected": room.choice_ids[0] }))
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
                       "points": 5, "choices": [{"id": "a", "text": "5"}, {"id": "b", "text": "6"}], "correct": "b" }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let question_id = question["id"].as_str().unwrap().to_string();
    let opts = choice_ids(&question);

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
        json!({ "type": "answer", "question_id": question_id, "selected": opts[1] }),
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

// --- chatbot SSE ---------------------------------------------------------
//
// The full production topology in one process: a browser-shaped HTTP client
// over real TCP, the backend, a real QUIC bridge, and an AI service dialled in
// from outside. What is pinned is what an `EventSource` actually observes —
// `delta`s then a `done` — including for the client that connects *after* the
// answer already landed, which must not hang waiting for a stream of an event
// that has been and gone.

use hezarfen_backend::ai::protocol::Response as AiResponse;
use hezarfen_backend::ai::protocol::{
    Greeting, Hello, Request as AiRequest, read_frame, write_frame,
};
use hezarfen_backend::ai::{AiBridge, BridgeConfig};
use hezarfen_backend::constant::{AI_ALPN, AI_CHAT_CAPABILITY, AI_PROTOCOL};
use std::time::Duration;

const AI_TOKEN: &str = "e2e-ai-token";

/// A bridge with a fake `chat.reply` service dialled into it, answering every
/// turn with `text`. Holding it keeps the registration alive.
struct ChatService {
    bridge: AiBridge,
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
    _control: (quinn::SendStream, quinn::RecvStream),
}

async fn chat_service(text: &str) -> ChatService {
    let bridge = AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: AI_TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: Duration::from_secs(10),
    })
    .await
    .expect("bridge binds on an ephemeral port");

    // Pin the bridge's own certificate, exactly as a production service does.
    hezarfen_backend::ai::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(bridge.certificate()).expect("pin the leaf");
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC-usable TLS"),
    ));
    let mut transport = quinn::TransportConfig::default();
    // Requests arrive as server-initiated streams.
    transport.max_concurrent_bidi_streams(64u32.into());
    config.transport_config(std::sync::Arc::new(transport));
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
    endpoint.set_default_client_config(config);

    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .expect("dial")
        .await
        .expect("QUIC handshake");
    let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
    write_frame(
        &mut send,
        &Hello {
            protocol: AI_PROTOCOL.to_string(),
            service: "e2e-tutor".to_string(),
            capabilities: vec![AI_CHAT_CAPABILITY.to_string()],
            token: AI_TOKEN.to_string(),
            max_concurrent: None,
        },
    )
    .await
    .expect("send Hello");
    let greeting: Greeting = read_frame(&mut recv).await.expect("read Greeting");
    assert!(matches!(greeting, Greeting::Welcome { .. }), "{greeting:?}");

    let answer = text.to_string();
    let serving = conn.clone();
    tokio::spawn(async move {
        while let Ok((mut send, mut recv)) = serving.accept_bi().await {
            let answer = answer.clone();
            tokio::spawn(async move {
                let Ok(request) = read_frame::<_, AiRequest>(&mut recv).await else {
                    return;
                };
                let response = AiResponse::Ok {
                    id: request.id.clone(),
                    school: request.school.clone(),
                    payload: json!({ "text": answer }),
                };
                let _ = write_frame(&mut send, &response).await;
                let _ = send.finish();
                let _ = send.stopped().await;
            });
        }
    });

    // Registration completes after the welcome is on the wire.
    for _ in 0..300 {
        if bridge.has_capability(AI_CHAT_CAPABILITY) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        bridge.has_capability(AI_CHAT_CAPABILITY),
        "the fake service never registered"
    );

    ChatService {
        bridge,
        _endpoint: endpoint,
        _conn: conn,
        _control: (send, recv),
    }
}

/// Open a thread and ask one question. Returns (thread id, reserved answer id).
async fn ask(client: &Client, base: &str) -> (String, String) {
    let thread: Value = client
        .post(format!("{base}/chatbot/threads"))
        .json(&json!({ "title": "Fizik" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let thread = thread["id"].as_str().expect("thread id").to_string();

    let res = client
        .post(format!("{base}/chatbot/threads/{thread}/messages"))
        .json(&json!({ "content": "ikinci yasa nedir?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let receipt: Value = res.json().await.unwrap();
    assert_eq!(receipt["status"], "pending");
    let mid = receipt["message_id"]
        .as_str()
        .expect("message id")
        .to_string();
    (thread, mid)
}

/// Read the SSE stream until its terminal event (`done` or `error`), returning
/// every `(event, data)` pair in arrival order. Bounded: a stream that never
/// terminates fails the test instead of hanging the suite.
async fn read_sse_to_end(res: &mut reqwest::Response, within: Duration) -> Vec<(String, Value)> {
    let deadline = tokio::time::Instant::now() + within;
    let mut buf = String::new();
    let mut events = Vec::new();
    loop {
        while let Some(end) = buf.find("\n\n") {
            let frame: String = buf.drain(..end + 2).collect();
            let Some(name) = frame
                .lines()
                .find_map(|line| line.strip_prefix("event:"))
                .map(|name| name.trim().to_string())
            else {
                continue; // a keep-alive comment
            };
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data:"))
                .expect("every chat event carries a data line")
                .trim();
            let terminal = name == "done" || name == "error";
            events.push((
                name,
                serde_json::from_str(data).expect("event data is json"),
            ));
            if terminal {
                return events;
            }
        }
        let chunk = tokio::time::timeout_at(deadline, res.chunk())
            .await
            .expect("the stream terminates before the deadline")
            .expect("stream stays open")
            .expect("stream yields data");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// The answer the fake service gives: long enough that the relay cuts it into
/// several `delta`s rather than emitting it whole.
fn long_answer() -> String {
    "kuvvet kutle carpi ivmedir. ".repeat(8)
}

/// Assert one full chat stream: `delta`s that rejoin into `answer`, then a
/// `done` carrying the finished message.
fn assert_deltas_then_done(events: &[(String, Value)], mid: &str, answer: &str) {
    let (last, deltas) = events.split_last().expect("at least a terminal event");
    assert!(!deltas.is_empty(), "no delta arrived: {events:?}");
    assert!(deltas.iter().all(|(name, _)| name == "delta"), "{events:?}");
    let text: String = deltas
        .iter()
        .map(|(_, data)| data["text"].as_str().expect("delta carries text"))
        .collect();
    assert_eq!(text, answer, "the deltas must rejoin into the answer");

    assert_eq!(last.0, "done", "{events:?}");
    let message = &last.1["message"];
    assert_eq!(message["id"], mid);
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["status"], "complete");
    assert_eq!(message["content"], answer);
    assert_eq!(message["truncated"], false, "{message}");
    assert!(message["error_code"].is_null(), "{message}");
}

/// The SSE `done` payload and the polling read are contractually required to
/// agree in every state — including about a clipped answer. A frontend that
/// streams must be able to tell the user the text was cut, and one that polls
/// must reach the same conclusion about the very same turn.
#[tokio::test]
async fn chat_stream_and_poll_agree_that_an_answer_was_truncated() {
    let cap = hezarfen_backend::constant::DEFAULT_MAX_CHATBOT_MESSAGE_LEN as usize;
    let service = chat_service(&"é".repeat(cap + 500)).await;
    let (base, _db) = spawn_server_with_ai(Some(service.bridge.clone())).await;
    let ali = client();
    register(&ali, &base, "ali").await;
    login(&ali, &base, "ali").await;

    let (thread, mid) = ask(&ali, &base).await;

    let mut res = ali
        .get(format!(
            "{base}/chatbot/threads/{thread}/messages/{mid}/stream"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let events = read_sse_to_end(&mut res, Duration::from_secs(10)).await;
    let (name, data) = events.last().expect("a terminal event");
    assert_eq!(name, "done", "{events:?}");
    let streamed = &data["message"];
    assert_eq!(streamed["status"], "complete", "{streamed}");
    assert_eq!(streamed["truncated"], true, "{streamed}");
    assert_eq!(
        streamed["content"]
            .as_str()
            .expect("content")
            .chars()
            .count(),
        cap
    );

    // The same row, read the other way: identical verdict, field for field.
    let polled: Value = ali
        .get(format!("{base}/chatbot/threads/{thread}/messages/{mid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(&polled, streamed, "the two reads must not disagree");
}

#[tokio::test]
async fn chat_stream_delivers_deltas_then_done_over_http() {
    let answer = long_answer();
    let service = chat_service(&answer).await;
    let (base, _db) = spawn_server_with_ai(Some(service.bridge.clone())).await;
    let ali = client();
    register(&ali, &base, "ali").await;
    login(&ali, &base, "ali").await;

    let (thread, mid) = ask(&ali, &base).await;

    // The client opens the stream while the turn is still in flight; it stays
    // open until the answer lands, then closes after `done`.
    let mut res = ali
        .get(format!(
            "{base}/chatbot/threads/{thread}/messages/{mid}/stream"
        ))
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

    let events = read_sse_to_end(&mut res, Duration::from_secs(10)).await;
    assert_deltas_then_done(&events, &mid, &answer);

    // The stream is closed after `done` — one stream per turn, not per thread.
    assert!(
        res.chunk().await.unwrap().is_none(),
        "the stream must close after done"
    );
}

#[tokio::test]
async fn chat_stream_replays_an_answer_that_already_landed() {
    // A client that reconnects late (reload, dropped connection, a second tab)
    // gets the same delta*+done as one that was watching all along. Nothing is
    // "missed": the stream reads the row, not a live feed.
    let answer = long_answer();
    let service = chat_service(&answer).await;
    let (base, _db) = spawn_server_with_ai(Some(service.bridge.clone())).await;
    let ali = client();
    register(&ali, &base, "ali").await;
    login(&ali, &base, "ali").await;

    let (thread, mid) = ask(&ali, &base).await;

    // Wait — by polling, never by sleeping — until the turn has settled, so the
    // stream below opens strictly after the answer landed.
    let mut settled = Value::Null;
    for _ in 0..500 {
        let turn: Value = ali
            .get(format!("{base}/chatbot/threads/{thread}/messages/{mid}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if turn["status"] != "pending" {
            settled = turn;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(settled["status"], "complete", "{settled}");
    assert_eq!(settled["content"], answer);

    let mut res = ali
        .get(format!(
            "{base}/chatbot/threads/{thread}/messages/{mid}/stream"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let events = read_sse_to_end(&mut res, Duration::from_secs(10)).await;
    assert_deltas_then_done(&events, &mid, &answer);
}

// ---- the board room (WebSocket) ---------------------------------------------
//
// The room is a fan-out protocol over stateful, append-only storage, so every
// test below asserts against the DATABASE as well as the wire: the in-memory
// engine forges concurrent-write wins (src/domain/cap.rs:44-49), and a frame
// proves only that the server said something.

/// A booted server with creator `ali`, invited `veli`, outsider `ayse` and one
/// open board — the spine of every board-room test.
struct BoardRoom {
    base: String,
    db: Database,
    creator: Client,
    creator_cookie: String,
    veli_cookie: String,
    ayse_cookie: String,
    veli_id: String,
    board_id: String,
}

async fn board_room_fixture() -> BoardRoom {
    let (base, db) = spawn_server().await;
    let creator = client();
    register(&creator, &base, "ali").await;
    login(&creator, &base, "ali").await;
    let veli = client();
    register(&veli, &base, "veli").await;
    login(&veli, &base, "veli").await;
    let ayse = client();
    register(&ayse, &base, "ayse").await;
    login(&ayse, &base, "ayse").await;

    let me: Value = veli
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let veli_id = me["id"].as_str().unwrap().to_string();

    let res = creator
        .post(format!("{base}/boards"))
        .json(&json!({ "title": "Geometri", "participants": [veli_id] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let board: Value = res.json().await.unwrap();
    let board_id = board["id"].as_str().unwrap().to_string();

    BoardRoom {
        creator_cookie: raw_session_cookie(&base, "ali").await,
        veli_cookie: raw_session_cookie(&base, "veli").await,
        ayse_cookie: raw_session_cookie(&base, "ayse").await,
        base,
        db,
        creator,
        veli_id,
        board_id,
    }
}

/// Open the board room. `Ok` is the upgraded socket; `Err` is the HTTP status
/// a pre-upgrade gate refused with — the room must reject before the upgrade,
/// so a refusal is a status and not an instant close.
async fn board_open(base: &str, board_id: &str, cookie: Option<&str>) -> Result<WsStream, u16> {
    let url = format!("{}/boards/{board_id}/ws", base.replace("http://", "ws://"));
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

/// Join and drain the replay, returning the strokes of the current epoch in
/// the order they arrived across however many `strokes` chunks it took.
async fn board_join(ws: &mut WsStream, after: Option<&str>, epoch: Option<i64>) -> Vec<Value> {
    let mut frame = json!({ "type": "join" });
    if let Some(after) = after {
        frame["after"] = json!(after);
    }
    if let Some(epoch) = epoch {
        frame["epoch"] = json!(epoch);
    }
    ws_send(ws, frame).await;
    drain_replay(ws).await
}

/// Every stroke of a replay, from wherever the stream is now until `synced`.
async fn drain_replay(ws: &mut WsStream) -> Vec<Value> {
    let mut strokes = Vec::new();
    loop {
        let frame = ws_next_frame(ws).await.expect("room closed mid-replay");
        match frame["type"].as_str().unwrap() {
            "strokes" => strokes.extend(frame["strokes"].as_array().unwrap().clone()),
            "synced" => return strokes,
            // `state` ticks and other people's frames may interleave a replay.
            _ => continue,
        }
    }
}

/// The next frame of `kind`, skipping anything else. Unlike the exam room's
/// helper this tolerates every other frame type: a board room is multi-writer,
/// so someone else's stroke can always land mid-wait.
async fn board_frame_of_type(ws: &mut WsStream, kind: &str) -> Value {
    loop {
        let frame = ws_next_frame(ws)
            .await
            .unwrap_or_else(|| panic!("room closed while waiting for a {kind:?} frame"));
        if frame["type"] == kind {
            return frame;
        }
    }
}

fn board_record(board: &str) -> surrealdb::types::RecordId {
    surrealdb::types::RecordId::new("board", board.to_string())
}

/// The stored stroke ids of one epoch, in mint order, read straight out of the
/// database — the only proof that survives a lying frame.
async fn stored_stroke_ids(db: &Database, board: &str, epoch: i64) -> Vec<String> {
    let mut result = db
        .query(
            "SELECT VALUE record::id(id) FROM board_stroke \
             WHERE board = $b AND epoch = $e AND kind = 'stroke' ORDER BY id",
        )
        .bind(("b", board_record(board)))
        .bind(("e", epoch))
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// The stored payloads of one epoch, in mint order.
async fn stored_payloads(db: &Database, board: &str, epoch: i64) -> Vec<String> {
    let mut result = db
        .query(
            "SELECT VALUE payload FROM board_stroke \
             WHERE board = $b AND epoch = $e AND kind = 'stroke' ORDER BY id",
        )
        .bind(("b", board_record(board)))
        .bind(("e", epoch))
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Draw one mark and wait for its ack, ignoring the fan-out of everyone else's
/// strokes that may arrive first.
async fn board_draw(ws: &mut WsStream, payload: &str) -> Value {
    ws_send(ws, json!({ "type": "stroke", "payload": payload })).await;
    board_frame_of_type(ws, "saved").await
}

/// Test 1 — the core of the feature: what one participant draws reaches the
/// other, and is in the database afterwards. Kills the class where the room is
/// a chat relay: frames fly, nothing persists, and a reload loses the canvas.
#[tokio::test]
async fn a_stroke_reaches_the_other_socket_and_the_database() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("creator upgrade");
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("participant upgrade");
    // Connect-time state names the room, its epoch and its roster.
    let state = board_frame_of_type(&mut veli, "state").await;
    assert_eq!(state["board"], *board);
    assert_eq!(state["epoch"], 0);
    assert_eq!(state["locked"], false);
    assert_eq!(state["participants"][0], room.veli_id);

    assert_eq!(board_join(&mut veli, None, None).await.len(), 0);

    // ali draws; veli sees it.
    ws_send(
        &mut ali,
        json!({ "type": "stroke", "payload": "{\"p\":[1,2]}", "client_seq": 41 }),
    )
    .await;
    let saved = board_frame_of_type(&mut ali, "saved").await;
    assert_eq!(saved["client_seq"], 41);
    let fanned = board_frame_of_type(&mut veli, "stroke").await;
    assert_eq!(fanned["payload"], "{\"p\":[1,2]}");
    assert_eq!(fanned["id"], saved["id"]);
    assert!(fanned["author"].as_str().is_some());

    // And the other way round, so fan-out is not one-directional by accident.
    // ali's next `stroke` frame is veli's mark and not the echo of its own:
    // an author gets `saved`, never its own stroke back, or every client would
    // have to filter the marks it just drew itself.
    board_draw(&mut veli, "{\"p\":[3,4]}").await;
    let fanned = board_frame_of_type(&mut ali, "stroke").await;
    assert_eq!(fanned["payload"], "{\"p\":[3,4]}");

    // THE assertion: stored state, re-read from the database.
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["{\"p\":[1,2]}".to_string(), "{\"p\":[3,4]}".to_string()],
        "the database is the source of truth, not the channel"
    );

    // A third socket joining now replays exactly those two, in mint order.
    let mut late = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("second participant socket");
    let replayed = board_join(&mut late, None, None).await;
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0]["payload"], "{\"p\":[1,2]}");
    assert_eq!(replayed[1]["payload"], "{\"p\":[3,4]}");
}

/// Test 2 — the permission edge that must NOT kill the socket: a participant
/// who is not the creator may not clear, and stays in the room drawing. Kills
/// the "refuse by closing" reflex, which would boot a whole class off the
/// board on one stray click.
#[tokio::test]
async fn a_participants_clear_is_refused_and_the_socket_keeps_drawing() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    board_join(&mut veli, None, None).await;
    board_draw(&mut veli, "before").await;

    ws_send(&mut veli, json!({ "type": "clear" })).await;
    let error = board_frame_of_type(&mut veli, "error").await;
    assert_eq!(error["code"], "forbidden");

    // Alive, and still a full participant.
    ws_send(&mut veli, json!({ "type": "ping" })).await;
    board_frame_of_type(&mut veli, "pong").await;
    board_draw(&mut veli, "after").await;
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["before".to_string(), "after".to_string()]
    );
    // The refusal wrote nothing: no clear marker, and the epoch never moved.
    let mut result = room
        .db
        .query("SELECT VALUE epoch FROM $b")
        .bind(("b", board_record(board)))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(result.take::<Vec<i64>>(0).unwrap(), vec![0]);
    let mut result = room
        .db
        .query("SELECT VALUE id FROM board_stroke WHERE kind = 'clear'")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        result
            .take::<Vec<surrealdb::types::RecordId>>(0)
            .unwrap()
            .is_empty()
    );

    // A lock is creator-only on the same terms, and equally non-fatal.
    ws_send(&mut veli, json!({ "type": "lock", "locked": true })).await;
    assert_eq!(
        board_frame_of_type(&mut veli, "error").await["code"],
        "forbidden"
    );
    board_draw(&mut veli, "still here").await;
}

/// Test 3 — the user's core requirement, unambiguous: a clear empties the LIVE
/// canvas and deletes nothing. A joiner sees only what came after; `/history`
/// still holds every mark drawn before. Kills the truncate-on-clear
/// implementation, which passes every happy-path test and destroys a lesson.
#[tokio::test]
async fn a_clear_empties_the_live_canvas_and_history_keeps_everything() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;
    board_draw(&mut ali, "old-1").await;
    board_draw(&mut ali, "old-2").await;

    ws_send(&mut ali, json!({ "type": "clear" })).await;
    let cleared = board_frame_of_type(&mut ali, "cleared").await;
    assert_eq!(cleared["epoch"], 1, "the clear opens the NEXT epoch");

    board_draw(&mut ali, "new-1").await;

    // A joiner gets the post-clear canvas and nothing else.
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    let replayed = board_join(&mut veli, None, None).await;
    assert_eq!(replayed.len(), 1, "join replays the current epoch only");
    assert_eq!(replayed[0]["payload"], "new-1");

    // Nothing was destroyed: both pre-clear strokes are still stored...
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["old-1".to_string(), "old-2".to_string()]
    );
    // ...and still served, with the marker that closed their epoch.
    let history: Value = room
        .creator
        .get(format!("{base}/boards/{board}/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds: Vec<&str> = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["stroke", "stroke", "clear", "stroke"]);
    assert_eq!(history["items"][0]["payload"], "old-1");
    assert_eq!(history["items"][2]["count"], 2, "the epoch's final count");
    assert_eq!(history["total"], 4);
}

/// Test 4 — reconnect with a cursor from an epoch that no longer exists. The
/// client must be told the canvas was wiped and be given the WHOLE current
/// epoch, never a diff from a cursor that means nothing now. Kills the
/// "resume from `after` regardless" bug, which paints a blank board forever.
#[tokio::test]
async fn a_stale_cursor_gets_a_cleared_frame_and_a_full_replay() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;
    board_draw(&mut ali, "old-1").await;
    let stale_cursor = board_draw(&mut ali, "old-2").await["id"]
        .as_str()
        .unwrap()
        .to_string();

    ws_send(&mut ali, json!({ "type": "clear" })).await;
    board_frame_of_type(&mut ali, "cleared").await;
    board_draw(&mut ali, "new-1").await;
    board_draw(&mut ali, "new-2").await;

    // The reconnect: an epoch-0 cursor against an epoch-1 board.
    let mut back = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("reconnect");
    ws_send(
        &mut back,
        json!({ "type": "join", "after": stale_cursor, "epoch": 0 }),
    )
    .await;
    let cleared = board_frame_of_type(&mut back, "cleared").await;
    assert_eq!(cleared["epoch"], 1, "wipe the stale canvas first");
    let replayed = drain_replay(&mut back).await;
    assert_eq!(
        replayed
            .iter()
            .map(|s| s["payload"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["new-1", "new-2"],
        "a full current-epoch replay, not a diff from a dead cursor"
    );

    // A cursor from the CURRENT epoch is still honoured as a cursor.
    let mut resume = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("reconnect");
    let first_new = stored_stroke_ids(&room.db, board, 1).await[0].clone();
    let replayed = board_join(&mut resume, Some(&first_new), Some(1)).await;
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0]["payload"], "new-2");
}

/// Test 5 — a subscriber that falls further behind than the hub's capacity
/// must NOT quietly lose strokes. The lag is forced (well past
/// `BOARD_HUB_CAPACITY` = 256 frames, each big enough to fill the socket
/// buffers), and the canvas the laggard ends up with is compared to the
/// database. Kills the `Err(_) => continue` handler, whose whole failure mode
/// is a permanently corrupt canvas on one client and no error anywhere.
#[tokio::test]
async fn a_lagged_socket_resyncs_to_exactly_what_the_database_holds() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    board_join(&mut veli, None, None).await;

    // veli stops reading. ali floods: 400 strokes of 4 KiB is ~1.6 MB, which
    // overruns the socket buffers, stalls the room's writer, and backs the
    // broadcast receiver up past its 256-frame capacity.
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;
    // In batches, acking each one: firing all 3000 first deadlocks the *test*
    // (ali stops reading, so ali's own socket fills and the server stops
    // reading ali), while one-at-a-time pays a round trip per stroke.
    //
    // 3000 x 4 KiB is ~12 MB. The number is empirical: the socket buffers
    // between the two ends swallowed ~1200 frames on this machine before the
    // room's writer stalled, and only then does the 256-frame channel start
    // dropping — so the count carries roughly 2x the measured margin.
    // corner-cut: a host with `net.ipv4.tcp_wmem` tuned far past the 4 MB
    // default could buffer more than this and the lag would stop being
    // forced; the test would then pass vacuously up to the resync assert,
    // which fails loudly rather than silently.
    let big = "x".repeat(4_000);
    let mut drawn = 0;
    while drawn < 3_000 {
        let batch = 50.min(3_000 - drawn);
        for n in 0..batch {
            ws_send(
                &mut ali,
                json!({ "type": "stroke", "payload": format!("{}:{big}", drawn + n) }),
            )
            .await;
        }
        for _ in 0..batch {
            board_frame_of_type(&mut ali, "saved").await;
        }
        drawn += batch;
    }
    let stored = stored_stroke_ids(&room.db, board, 0).await;
    assert_eq!(stored.len(), 3_000, "every flooded stroke persisted");

    // veli starts reading again: the room notices the gap and says so.
    let resync = loop {
        let frame = ws_next_frame(&mut veli).await.expect("room stays open");
        if frame["type"] == "error" {
            break frame;
        }
    };
    assert_eq!(
        resync["code"], "resync",
        "a dropped frame must be announced, never swallowed"
    );
    // ...and re-serves the current epoch in full.
    let replayed = drain_replay(&mut veli).await;
    assert_eq!(
        replayed
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        stored,
        "after a lag the client's canvas must equal the database's"
    );
}

/// Test 6 — the door. An outsider learns nothing: the same 404 as a board that
/// was never created, and before the upgrade so it is an HTTP status rather
/// than an instant close. Kills existence leaks (src/lib.rs:57).
#[tokio::test]
async fn the_door_is_a_404_for_an_outsider_and_an_unknown_board() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    assert_eq!(
        board_open(base, board, Some(&room.ayse_cookie)).await.err(),
        Some(404),
        "a non-participant must not learn the board exists"
    );
    assert_eq!(
        board_open(
            base,
            "01JZZZZZZZZZZZZZZZZZZZZZZZ",
            Some(&room.creator_cookie)
        )
        .await
        .err(),
        Some(404),
        "indistinguishable from an unknown board"
    );
    // An over-long key is length-checked before it can be echoed anywhere.
    assert_eq!(
        board_open(base, &"x".repeat(200), Some(&room.creator_cookie))
            .await
            .err(),
        Some(404)
    );
    // No cookie at all is a 401, not a silent upgrade.
    assert_eq!(board_open(base, board, None).await.err(), Some(401));
    // And the participant does get in, so the 404s above are about the caller.
    board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("a participant is let in");
}

/// Test 6b — the door is shut on a `parent` too, and shut the same way: a 404,
/// never a 403. Proved against the hardest case — the parent is forced onto the
/// roster in the database first, so the roster gate would let them in and only
/// the role bar refuses. No socket is the whole point: a parent that cannot
/// upgrade can never send a `stroke` frame, and the store proves nothing landed.
#[tokio::test]
async fn a_parent_cannot_enter_the_room_or_draw() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let anne = client();
    register(&anne, base, "anne").await;
    promote(&room.db, "anne", "parent").await;
    login(&anne, base, "anne").await;
    let cookie = raw_session_cookie(base, "anne").await;

    // On the roster by force — the stale row this rule leaves behind.
    let me: Value = anne
        .get(format!("{base}/auth/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Beside `veli`, who stays: the two differ only by role, so the refusal
    // below cannot be blamed on the roster.
    room.db
        .query("UPDATE $b SET participants = [$v, $u]")
        .bind(("b", board_record(board)))
        .bind((
            "v",
            surrealdb::types::RecordId::new("user", room.veli_id.clone()),
        ))
        .bind((
            "u",
            surrealdb::types::RecordId::new("user", me["id"].as_str().unwrap().to_string()),
        ))
        .await
        .unwrap()
        .check()
        .unwrap();

    assert_eq!(
        board_open(base, board, Some(&cookie)).await.err(),
        Some(404),
        "a parent on the roster must still be told the board does not exist"
    );
    // Not a 403 anywhere: identical to a board that was never minted.
    assert_eq!(
        board_open(base, "01JZZZZZZZZZZZZZZZZZZZZZZZ", Some(&cookie))
            .await
            .err(),
        Some(404)
    );
    // No socket, so no stroke — asserted against the store, not the wire.
    assert!(
        stored_stroke_ids(&room.db, board, 0).await.is_empty(),
        "a parent must not have drawn"
    );
    // And the bar is about the role, not the roster: `veli` sits on the same
    // roster and is let straight in.
    board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("a student participant is still let in");
}

/// Test 7 — a participant dropped mid-session stops drawing. Twice over: once
/// with the roster frame deliberately withheld (the socket's own gate is what
/// actually protects the board), and once through the REST route's fan-out —
/// which the removed socket never receives, because the authorization in front
/// of every delivery refuses it first.
#[tokio::test]
async fn a_removed_participant_can_no_longer_draw() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    board_join(&mut veli, None, None).await;
    board_draw(&mut veli, "while invited").await;

    // The silent removal first: write the roster straight into the database,
    // so no frame is published and only the per-stroke gate can catch it.
    room.db
        .query("UPDATE $b SET participants = []")
        .bind(("b", board_record(board)))
        .await
        .unwrap()
        .check()
        .unwrap();
    ws_send(
        &mut veli,
        json!({ "type": "stroke", "payload": "after removal" }),
    )
    .await;
    let error = board_frame_of_type(&mut veli, "error").await;
    assert_eq!(
        error["code"], "forbidden",
        "the stroke path re-reads the roster; it cannot trust a frame it may have missed"
    );
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["while invited".to_string()],
        "the refused stroke wrote nothing"
    );
    // Re-entry is the outsider's 404 now.
    assert_eq!(
        board_open(base, board, Some(&room.veli_cookie)).await.err(),
        Some(404)
    );

    // And the announced removal: re-invite, reconnect, then drop over REST.
    let res = room
        .creator
        .patch(format!("{base}/boards/{board}"))
        .json(&json!({ "participants": [room.veli_id] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("re-invited");
    board_join(&mut veli, None, None).await;
    let res = room
        .creator
        .patch(format!("{base}/boards/{board}"))
        .json(&json!({ "participants": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // The removed socket is *not* handed the new roster: authorization runs in
    // front of every delivery now, so what reaches it is its own refusal and
    // not one more fact about a board it is no longer on.
    let refusal = board_frame_of_type(&mut veli, "error").await;
    assert_eq!(refusal["code"], "forbidden", "{refusal}");
    assert_eq!(
        refusal["message"], "you are no longer on this board",
        "a roster removal must still explain itself — and say something other \
         than the role bar's refusal, or a client cannot tell them apart"
    );
    // And the room drops the socket rather than leaving it open and mute.
    loop {
        match ws_next_frame(&mut veli).await {
            None => break,
            Some(frame) => {
                assert_ne!(frame["type"], "saved", "a dropped socket must not save");
                assert_ne!(
                    frame["type"], "participants",
                    "a removed socket is told it is out, never who is in"
                );
            }
        }
    }
}

/// A bulk invite (`POST /boards/{id}/invite`) must reach the live room like any
/// other roster change. The room authorizes every delivery against the roster it
/// is told about, so a path that widens the array and stays silent leaves the
/// new participant unrenderable to the people already drawing — and this one is
/// easy to forget precisely because it only ever *adds*, so it can never trip
/// the removal test above.
#[tokio::test]
async fn a_bulk_invite_announces_the_widened_roster_to_the_room() {
    let room = board_room_fixture().await;
    let (base, board) = (room.base.as_str(), room.board_id.as_str());
    // The invite gates are teacher+, and the fixture's creator is a student.
    promote(&room.db, "ali", "teacher").await;

    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("invited");
    board_join(&mut veli, None, None).await;

    // A school-wide event resolves to every account, which here is the three the
    // fixture registered — so the invite adds exactly the outsider, ayşe.
    let res = room
        .creator
        .post(format!("{base}/events"))
        .json(&json!({ "title": "Tüm okul" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let event: Value = res.json().await.unwrap();
    let event_id = event["id"].as_str().unwrap().to_string();

    let res = room
        .creator
        .post(format!("{base}/boards/{board}/invite"))
        .json(&json!({ "kind": "event", "event": event_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let widened: Value = res.json().await.unwrap();

    let frame = board_frame_of_type(&mut veli, "participants").await;
    let announced = frame["participants"].as_array().unwrap();
    assert!(
        announced.iter().any(|id| id == &json!(room.veli_id)),
        "the standing participant must still be on the announced roster: {frame}"
    );
    assert_eq!(
        announced.len(),
        2,
        "the outsider should have been added and announced: {frame}"
    );
    // The wire and the reply are the same list, and the creator is in neither
    // array — they are a participant by construction.
    let mut from_frame: Vec<&Value> = announced.iter().collect();
    let served = widened["participants"].as_array().unwrap();
    let mut from_body: Vec<&Value> = served.iter().collect();
    from_frame.sort_by_key(|id| id.as_str().unwrap());
    from_body.sort_by_key(|id| id.as_str().unwrap());
    assert_eq!(from_frame, from_body, "{frame} vs {widened}");
    assert_eq!(frame["creator"], widened["creator"], "{frame}");

    // The stored row is the authority — the mem engine forges write wins, so a
    // frame alone proves only that the server said something.
    let stored: Value = room
        .creator
        .get(format!("{base}/boards/{board}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stored["participants"], widened["participants"]);
}

/// Test 8 — `client_seq` is the client's correlation id and is echoed
/// verbatim, including a value no `i32` holds; a message without one gets a
/// reply without the key at all. Kills the "helpfully normalize it" bug that
/// silently breaks in-flight matching (mirrors src/web/exam_ws.rs:506-510).
#[tokio::test]
async fn client_seq_is_echoed_verbatim_or_omitted() {
    let room = board_room_fixture().await;
    let mut ali = board_open(&room.base, &room.board_id, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;

    let huge: u64 = 18_446_744_073_709_551_615;
    ws_send(
        &mut ali,
        json!({ "type": "stroke", "payload": "a", "client_seq": huge }),
    )
    .await;
    let saved = board_frame_of_type(&mut ali, "saved").await;
    assert_eq!(saved["client_seq"].as_u64(), Some(huge));

    // A refusal carries it too, or an in-flight failure cannot be matched.
    ws_send(
        &mut ali,
        json!({ "type": "stroke", "payload": "", "client_seq": 5 }),
    )
    .await;
    let error = board_frame_of_type(&mut ali, "error").await;
    assert_eq!(error["client_seq"], 5);

    // Omitted stays omitted — never `null`.
    let saved = board_draw(&mut ali, "b").await;
    assert!(
        saved.get("client_seq").is_none(),
        "a client that sends no seq must see byte-identical frames: {saved}"
    );
}

/// Test 9 — replay across the `BOARD_REPLAY_CHUNK` (200) boundary: every
/// stroke exactly once, in mint order, no gap at the seam and no row served
/// twice. Kills both classic paging bugs (`>=` vs `>` on the cursor, and a
/// loop that stops at the first full chunk).
#[tokio::test]
async fn replay_crosses_the_chunk_boundary_exactly_once() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;
    for n in 0..205 {
        board_draw(&mut ali, &format!("mark-{n}")).await;
    }
    let stored = stored_stroke_ids(&room.db, board, 0).await;
    assert_eq!(stored.len(), 205);

    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    let replayed = board_join(&mut veli, None, None).await;
    let ids: Vec<String> = replayed
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, stored, "every stroke once, in mint order");
    assert_eq!(
        replayed
            .iter()
            .map(|s| s["payload"].as_str().unwrap())
            .collect::<Vec<_>>(),
        (0..205).map(|n| format!("mark-{n}")).collect::<Vec<_>>()
    );
}

/// Test 10 — the lock is a live pause, not a disconnect, and it reaches the
/// room whichever way it was thrown (socket or REST). Kills the cached-flag
/// implementation, where a client that missed the frame keeps drawing.
#[tokio::test]
async fn a_lock_pauses_drawing_for_everyone_and_a_thaw_resumes_it() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;
    board_join(&mut veli, None, None).await;

    ws_send(&mut ali, json!({ "type": "lock", "locked": true })).await;
    let locked = board_frame_of_type(&mut veli, "locked").await;
    assert_eq!(locked["locked"], true);

    ws_send(&mut veli, json!({ "type": "stroke", "payload": "sneaky" })).await;
    assert_eq!(
        board_frame_of_type(&mut veli, "error").await["code"],
        "locked"
    );
    assert!(stored_payloads(&room.db, board, 0).await.is_empty());

    // Thawed over REST this time: the room must re-read, not trust its cache.
    let res = room
        .creator
        .patch(format!("{base}/boards/{board}"))
        .json(&json!({ "locked": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        board_frame_of_type(&mut veli, "locked").await["locked"],
        false
    );
    board_draw(&mut veli, "allowed again").await;
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["allowed again".to_string()]
    );
}

/// Test 11 — the two terminal REST frames end the room. A closed board is
/// read-only but still readable; a deleted one is gone, and a socket that
/// outlives its board must not answer for it.
#[tokio::test]
async fn closing_and_deleting_the_board_end_the_room() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    board_join(&mut veli, None, None).await;
    board_draw(&mut veli, "before the close").await;

    let res = room
        .creator
        .post(format!("{base}/boards/{board}/close"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let closed = board_frame_of_type(&mut veli, "closed").await;
    assert!(closed["closed_at"].as_i64().is_some());
    assert!(
        ws_next_frame(&mut veli).await.is_none(),
        "a closed board ends the room"
    );
    // Read-only, not gone: the canvas still replays, drawing does not.
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("a closed board is still readable");
    assert_eq!(board_join(&mut veli, None, None).await.len(), 1);
    ws_send(
        &mut veli,
        json!({ "type": "stroke", "payload": "too late" }),
    )
    .await;
    assert_eq!(
        board_frame_of_type(&mut veli, "error").await["code"],
        "board_closed"
    );
    assert_eq!(stored_payloads(&room.db, board, 0).await.len(), 1);

    let res = room
        .creator
        .delete(format!("{base}/boards/{board}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    board_frame_of_type(&mut veli, "deleted").await;
    assert!(
        ws_next_frame(&mut veli).await.is_none(),
        "a deleted board ends the room"
    );
    assert!(stored_payloads(&room.db, board, 0).await.is_empty());
}

/// Test 12 — two sockets drawing at once. Every accepted stroke is in the
/// database exactly once and both clients can reach that same canvas: the
/// concurrent case the whole feature exists for.
#[tokio::test]
async fn simultaneous_drawing_lands_every_stroke_exactly_once() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;
    board_join(&mut veli, None, None).await;

    // Interleaved without waiting for acks, so the two writers really overlap.
    for n in 0..20 {
        ws_send(
            &mut ali,
            json!({ "type": "stroke", "payload": format!("ali-{n}") }),
        )
        .await;
        ws_send(
            &mut veli,
            json!({ "type": "stroke", "payload": format!("veli-{n}") }),
        )
        .await;
    }
    for _ in 0..20 {
        board_frame_of_type(&mut ali, "saved").await;
    }
    for _ in 0..20 {
        board_frame_of_type(&mut veli, "saved").await;
    }

    let stored = stored_payloads(&room.db, board, 0).await;
    assert_eq!(stored.len(), 40, "no stroke lost, none duplicated");
    let mut unique = stored.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 40);

    // A fresh joiner sees that exact canvas — the DB, not either socket's view.
    let mut late = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    assert_eq!(
        board_join(&mut late, None, None)
            .await
            .iter()
            .map(|s| s["payload"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        stored
    );
}

/// Test 13 — junk in, error out, room alive. A malformed frame or an unknown
/// type must never take the canvas down with it.
#[tokio::test]
async fn junk_frames_do_not_end_the_room() {
    let room = board_room_fixture().await;
    let mut ali = board_open(&room.base, &room.board_id, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;

    ws_send(&mut ali, json!({ "type": "nonsense" })).await;
    board_frame_of_type(&mut ali, "error").await;
    ali.send(Message::Text("{not json".into())).await.unwrap();
    board_frame_of_type(&mut ali, "error").await;
    // An oversized payload is refused by the domain's own gate.
    ws_send(
        &mut ali,
        json!({ "type": "stroke", "payload": "x".repeat(4_097) }),
    )
    .await;
    board_frame_of_type(&mut ali, "error").await;

    board_draw(&mut ali, "still fine").await;
    assert_eq!(
        stored_payloads(&room.db, &room.board_id, 0).await,
        vec!["still fine".to_string()]
    );
}

/// Test 14 (grafted from candidate B) — the half no other board test reaches:
/// a `participants` frame drops *only* the socket it removed. Test 7 proves the
/// removed one is dropped; nothing proved the survivors keep drawing, so a
/// `forward` that closed the room on every roster change would pass the suite.
#[tokio::test]
async fn a_roster_change_drops_only_the_socket_it_removed() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("upgrade");
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut veli, None, None).await;
    board_join(&mut ali, None, None).await;

    // The creator empties the invite list over REST.
    let res = room
        .creator
        .patch(format!("{base}/boards/{board}"))
        .json(&json!({ "participants": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The dropped participant is closed…
    loop {
        match ws_next_frame(&mut veli).await {
            None => break,
            Some(frame) => assert_ne!(frame["type"], "saved", "a dropped socket must not save"),
        }
    }
    // … while the creator, still on the board, sees the new roster and keeps
    // drawing on the very same socket.
    let roster = board_frame_of_type(&mut ali, "participants").await;
    assert!(roster["participants"].as_array().unwrap().is_empty());
    board_draw(&mut ali, "still mine").await;
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["still mine".to_string()]
    );
}

/// Test 15 — clearing a blank canvas is a *live* board saying "nothing to do",
/// and the room must say so with a non-terminal code. It shipped as
/// `board_closed` — the terminal one — because the classifier guessed the code
/// from a substring of the refusal's words, and the reworded refusal happens to
/// contain "closed". A creator who double-clicks clear would have been told the
/// board was finished forever, on a board still open and still drawable.
#[tokio::test]
async fn clearing_a_blank_canvas_never_tells_the_room_the_board_is_closed() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("upgrade");
    board_join(&mut ali, None, None).await;

    // Nothing drawn yet: there is no epoch to close.
    ws_send(&mut ali, json!({ "type": "clear" })).await;
    let error = board_frame_of_type(&mut ali, "error").await;
    assert_eq!(error["code"], "canvas_blank", "{error}");

    // The same refusal after a real clear — the second press of the button.
    board_draw(&mut ali, "one").await;
    ws_send(&mut ali, json!({ "type": "clear" })).await;
    board_frame_of_type(&mut ali, "cleared").await;
    ws_send(&mut ali, json!({ "type": "clear" })).await;
    assert_eq!(
        board_frame_of_type(&mut ali, "error").await["code"],
        "canvas_blank"
    );

    // And the board really is live: the socket draws on, into the new epoch.
    board_draw(&mut ali, "two").await;
    assert_eq!(
        stored_payloads(&room.db, board, 1).await,
        vec!["two".to_string()]
    );
}

/// The class layer over real HTTP with three cookie jars: the office builds the
/// class, the teacher hands over their own course, and the student — who was
/// never enrolled by anyone — finds it on `GET /courses/me`. Taking them out of
/// the class takes the seat back with them.
#[tokio::test]
async fn a_class_seats_its_roster_and_gives_the_seat_back() {
    let (base, db) = spawn_server().await;
    let manager = client();
    let teacher = client();
    let student = client();

    for (c, name) in [(&manager, "mgr"), (&teacher, "tch"), (&student, "veli")] {
        assert_eq!(register(c, &base, name).await.status(), StatusCode::CREATED);
    }
    promote(&db, "mgr", "manager").await;
    promote(&db, "tch", "teacher").await;
    for (c, name) in [(&manager, "mgr"), (&teacher, "tch"), (&student, "veli")] {
        login(c, &base, name).await;
    }

    let id_of = |v: &Value| v["id"].as_str().unwrap().to_string();
    let student_id = id_of(
        &student
            .get(format!("{base}/auth/me"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
    );

    // The office opens the class and puts the student in it.
    let res = manager
        .post(format!("{base}/classes"))
        .json(&json!({ "name": "9-A", "grade": "9" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    // `POST /classes` answers `{class, skipped, stocked_from}` — it stocks the
    // new section from its grade's blueprint, and there is none here.
    let class = id_of(&res.json::<Value>().await.unwrap()["class"]);
    let res = manager
        .post(format!("{base}/classes/{class}/members"))
        .json(&json!({ "user_id": student_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // The teacher hands their own course to the class …
    let res = teacher
        .post(format!("{base}/courses"))
        .json(&json!({ "title": "Cebir" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let course = id_of(&res.json::<Value>().await.unwrap());
    let res = teacher
        .post(format!("{base}/classes/{class}/courses"))
        .json(&json!({ "course_id": course }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);

    // … and the student, whom nobody ever enrolled, is in it.
    let mine: Value = student
        .get(format!("{base}/courses/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine["total"], 1, "the class seated them: {mine}");
    assert_eq!(mine["items"][0]["id"], course);
    assert_eq!(mine["items"][0]["title"], "Cebir");

    // Out of the class, out of the course — the seat comes back.
    let res = manager
        .delete(format!("{base}/classes/{class}/members/{student_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let mine: Value = student
        .get(format!("{base}/courses/me"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mine["total"], 0, "the seat went with them: {mine}");
    let roster: Value = teacher
        .get(format!("{base}/courses/{course}/enrollments"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(roster["total"], 0, "and off the teacher's roster: {roster}");
}

/// The role bar has to close READS, not only writes. `forward` used to hand the
/// frame to the socket first and authorize afterwards — and only on a
/// `participants` frame or the 15-second state tick — so a participant demoted
/// to `parent`, the role with no whiteboard access at all, kept being served
/// every mark drawn on a live canvas until the tick caught up. Driven over a
/// real socket, because the leak lives in the fan-out and nowhere else.
///
/// The role is moved straight in the database, which is both the school's
/// documented bootstrap and the harshest case: no `participants` frame is
/// published to prompt the room, so only the authorization in front of the send
/// can refuse it.
#[tokio::test]
async fn a_demoted_participant_stops_reading_the_canvas_at_once() {
    let room = board_room_fixture().await;
    let (base, board) = (&room.base, &room.board_id);
    let mut ali = board_open(base, board, Some(&room.creator_cookie))
        .await
        .expect("creator upgrade");
    let mut veli = board_open(base, board, Some(&room.veli_cookie))
        .await
        .expect("participant upgrade");
    board_join(&mut veli, None, None).await;

    // Still a student: the fan-out reaches them.
    board_draw(&mut ali, "{\"p\":[1,2]}").await;
    let fanned = board_frame_of_type(&mut veli, "stroke").await;
    assert_eq!(fanned["payload"], "{\"p\":[1,2]}");

    promote(&room.db, "veli", "parent").await;
    board_draw(&mut ali, "{\"p\":[3,4]}").await;

    // The next thing that socket hears is its refusal, and the room ends. It
    // must never be the mark: `parent` has no whiteboard access whatsoever.
    let mut refused = false;
    while let Some(frame) = ws_next_frame(&mut veli).await {
        assert_ne!(
            frame["type"], "stroke",
            "a demoted participant read live canvas content: {frame}"
        );
        if frame["type"] == "error" {
            assert_eq!(frame["code"], "forbidden", "{frame}");
            // Its own words, distinct from a roster removal's: the client can
            // tell "you were taken off this board" from "your account may no
            // longer use boards at all".
            assert_eq!(frame["message"], "your role can no longer use boards");
            refused = true;
        }
    }
    assert!(refused, "the socket must be told why it was dropped");

    // Nothing was destroyed by the refusal, and the room carries on for the
    // people still entitled to it.
    assert_eq!(
        stored_payloads(&room.db, board, 0).await,
        vec!["{\"p\":[1,2]}".to_string(), "{\"p\":[3,4]}".to_string()],
    );
    board_draw(&mut ali, "{\"p\":[5,6]}").await;
}

/// A manager archives an academic year and the whole year turns read-only from
/// the user's seat: every write into its structure answers `409 term_archived`,
/// every read still answers, the live exam room's door refuses the upgrade with
/// a real HTTP 409 (before the WebSocket handshake completes), and unarchiving
/// reopens all of it. The two archive routes are discoverable in the served
/// OpenAPI document Swagger renders.
#[tokio::test]
async fn a_manager_archived_year_refuses_every_write_and_answers_every_read() {
    let (base, db) = spawn_server().await;
    let mudur = client();
    let ogrenci = client();
    let kaan = client();
    register(&mudur, &base, "mudur").await;
    register(&ogrenci, &base, "ogrenci").await;
    register(&kaan, &base, "kaan").await;
    promote(&db, "mudur", "manager").await;
    login(&mudur, &base, "mudur").await;
    login(&ogrenci, &base, "ogrenci").await;
    login(&kaan, &base, "kaan").await;

    let json_of = async |res: reqwest::Response| res.json::<Value>().await.unwrap();
    let student_id =
        json_of(ogrenci.get(format!("{base}/auth/me")).send().await.unwrap()).await["id"]
            .as_str()
            .unwrap()
            .to_string();
    let kaan_id = json_of(kaan.get(format!("{base}/auth/me")).send().await.unwrap()).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let now = json_of(mudur.get(format!("{base}/time")).send().await.unwrap()).await["now"]
        .as_i64()
        .unwrap();

    // --- a year's worth of structure, all hanging off one term -------------
    let res = mudur
        .post(format!("{base}/terms"))
        .json(&json!({ "name": "2026 Fall", "starts_at": now - 86_400_000, "ends_at": now + 86_400_000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let term: Value = res.json().await.unwrap();
    let term_id = term["id"].as_str().unwrap().to_string();
    assert!(
        term["archived_at"].is_null(),
        "a fresh term is open: {term}"
    );

    let res = mudur
        .post(format!("{base}/courses"))
        .json(&json!({ "title": "algebra", "term_id": term_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let course_id = json_of(res).await["id"].as_str().unwrap().to_string();

    let subject_id = json_of(
        mudur
            .post(format!("{base}/courses/{course_id}/subjects"))
            .json(&json!({ "name": "arithmetic" }))
            .send()
            .await
            .unwrap(),
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let exam: Value = json_of(
        mudur
            .post(format!("{base}/courses/{course_id}/exams"))
            .json(&json!({ "title": "midterm", "kind": "quiz", "mode": "open", "max_attempts": 2 }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let exam_id = exam["id"].as_str().unwrap().to_string();
    let question: Value = json_of(
        mudur
            .post(format!("{base}/exams/{exam_id}/questions"))
            .json(
                &json!({ "subject_id": subject_id, "text": "2 + 2?", "kind": "choice",
                           "points": 10,
                           "choices": [{"id": "a", "text": "3"}, {"id": "b", "text": "4"}],
                           "correct": "b" }),
            )
            .send()
            .await
            .unwrap(),
    )
    .await;
    let question_id = question["id"].as_str().unwrap().to_string();
    let opts = choice_ids(&question);

    let res = mudur
        .post(format!("{base}/courses/{course_id}/sessions"))
        .json(&json!({ "starts_at": now + 3_600_000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let session_id = json_of(res).await["id"].as_str().unwrap().to_string();

    let res = mudur
        .post(format!("{base}/courses/{course_id}/homework"))
        .json(
            &json!({ "title": "read ch3", "subject_id": subject_id, "due_at": now + 604_800_000 }),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let homework_id = json_of(res).await["id"].as_str().unwrap().to_string();

    let res = mudur
        .post(format!("{base}/course-notes"))
        .json(&json!({ "course": course_id, "title": "recap", "content": "quadratics" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let note_id = json_of(res).await["id"].as_str().unwrap().to_string();

    let res = mudur
        .post(format!("{base}/classes"))
        .json(&json!({ "name": "9-A", "grade": "9", "term_id": term_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let class_id = json_of(res).await["class"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let res = mudur
        .post(format!("{base}/courses/{course_id}/enrollments"))
        .json(&json!({ "user_id": student_id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The student is mid-sitting when the year closes: a live attempt whose
    // room door is about to be walled.
    let res = ogrenci
        .post(format!("{base}/exams/{exam_id}/attempt"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED, "start the live sitting");
    let student_cookie = raw_session_cookie(&base, "ogrenci").await;

    // --- archive ----------------------------------------------------------
    let res = mudur
        .post(format!("{base}/terms/{term_id}/archive"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let archived: Value = res.json().await.unwrap();
    let stamp = archived["archived_at"].as_i64().expect("archived_at stamp");

    // Idempotent: a second archive answers 200 with the stamp it already had.
    let res = mudur
        .post(format!("{base}/terms/{term_id}/archive"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        json_of(res).await["archived_at"].as_i64(),
        Some(stamp),
        "re-archiving must not move the stamp"
    );

    // --- every write into the closed year is a 409 `term_archived` ---------
    let refusals: Vec<(&str, reqwest::Response)> = vec![
        (
            "PATCH /terms/{id}",
            mudur
                .patch(format!("{base}/terms/{term_id}"))
                .json(&json!({ "name": "renamed" }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "PATCH /courses/{id}",
            mudur
                .patch(format!("{base}/courses/{course_id}"))
                .json(&json!({ "title": "renamed" }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "POST /courses/{id}/enrollments",
            mudur
                .post(format!("{base}/courses/{course_id}/enrollments"))
                .json(&json!({ "user_id": kaan_id }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "POST /sessions/{id}/attendance",
            mudur
                .post(format!("{base}/sessions/{session_id}/attendance"))
                .json(&json!({ "status": "present", "user_id": student_id }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "PATCH /exams/{id}",
            mudur
                .patch(format!("{base}/exams/{exam_id}"))
                .json(&json!({ "title": "renamed" }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "POST /exams/{id}/attempt/answers",
            ogrenci
                .post(format!("{base}/exams/{exam_id}/attempt/answers"))
                .json(&json!({ "question_id": question_id, "selected": opts[1] }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "POST /homework/{id}/results",
            mudur
                .post(format!("{base}/homework/{homework_id}/results"))
                .json(&json!({ "user": student_id, "status": "done", "mark": 90 }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "PATCH /course-notes/{id}",
            mudur
                .patch(format!("{base}/course-notes/{note_id}"))
                .json(&json!({ "title": "renamed" }))
                .send()
                .await
                .unwrap(),
        ),
        (
            "POST /classes/{id}/members",
            mudur
                .post(format!("{base}/classes/{class_id}/members"))
                .json(&json!({ "user_id": kaan_id }))
                .send()
                .await
                .unwrap(),
        ),
    ];
    for (what, res) in refusals {
        let status = res.status();
        let body: Value = res.json().await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{what} status: {body}");
        assert_eq!(body["code"], "term_archived", "{what} code: {body}");
    }

    // --- every read still answers -----------------------------------------
    for url in [
        format!("{base}/terms/{term_id}"),
        format!("{base}/courses/{course_id}"),
        format!("{base}/exams/{exam_id}"),
        format!("{base}/sessions/{session_id}"),
        format!("{base}/homework/{homework_id}"),
        format!("{base}/course-notes/{note_id}"),
        format!("{base}/classes/{class_id}"),
    ] {
        let res = mudur.get(&url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "read stays open: {url}");
    }

    // --- the exam room's door: refused before the upgrade ------------------
    let refused = ws_open(&base, &exam_id, Some(&student_cookie))
        .await
        .expect_err("an archived year's room must refuse the handshake");
    assert_eq!(refused, 409, "the room door answers a real HTTP 409");

    // --- unarchive reopens the year ---------------------------------------
    let res = mudur
        .post(format!("{base}/terms/{term_id}/unarchive"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let reopened: Value = res.json().await.unwrap();
    assert!(
        reopened["archived_at"].is_null(),
        "unarchive clears the stamp: {reopened}"
    );

    // A write that was refused a moment ago now lands.
    let res = mudur
        .patch(format!("{base}/courses/{course_id}"))
        .json(&json!({ "title": "algebra II" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "the reopened year takes writes"
    );

    // The room door opens again, and the live sitting is still there.
    let mut ws = ws_open(&base, &exam_id, Some(&student_cookie))
        .await
        .expect("the reopened year's room");
    let state = ws_next_frame(&mut ws).await.expect("connect state frame");
    assert_eq!(state["type"], "state", "{state}");

    // Archive under the open socket: `finish` comes back as an error frame
    // carrying the archived refusal instead of submitting the sheet.
    let res = mudur
        .post(format!("{base}/terms/{term_id}/archive"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    ws_send(&mut ws, json!({ "type": "finish" })).await;
    let error = ws_frame_of_type(&mut ws, "error").await;
    assert!(
        error["message"].as_str().unwrap().contains("archived"),
        "the room's finish refusal names the archived year: {error}"
    );

    // Reopen once more and the same frame submits the sitting.
    let res = mudur
        .post(format!("{base}/terms/{term_id}/unarchive"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    ws_send(&mut ws, json!({ "type": "finish" })).await;
    let finished = ws_frame_of_type(&mut ws, "finished").await;
    assert!(finished["finished_at"].as_i64().is_some(), "{finished}");

    // --- discoverable in the document Swagger renders ----------------------
    let spec: Value = json_of(
        mudur
            .get(format!("{base}/api-docs/openapi.json"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    for path in ["/terms/{id}/archive", "/terms/{id}/unarchive"] {
        assert!(
            spec["paths"][path]["post"].is_object(),
            "{path} must be a documented POST in the served spec"
        );
    }
}
