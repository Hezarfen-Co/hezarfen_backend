//! End-to-end tests for the podcast nest, against a fake podcast service.
//!
//! Everything here runs over a real QUIC socket on loopback — a real
//! handshake, real TLS, real streams — with [`Behaviour::Podcast`] standing in
//! for what the service does with a request. The **backend owns the job**: the
//! submit door mints the id, writes the `podcast_job` row, and only then
//! dispatches `podcast.submit`; the three reading doors answer from that row
//! alone; and the service reports every transition and uploads the finished
//! episode on its own client-initiated streams — the same connection it
//! registered with, exactly the shape `tests/ai_bridge.rs` drives.
//!
//! The assertions therefore split in two: what a browser gets from the real
//! router, and what the *service* received ([`FakeService::seen`]) or what the
//! school's own database and blob directory hold. A reading door that quietly
//! round-tripped to the service would fail the `seen` assertions; a submit that
//! did not write its row first would fail the database ones.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use hezarfen_backend::ai::protocol::{
    Greeting, Hello, Request, Response, read_frame, write_frame,
};
use hezarfen_backend::ai::{AiBridge, BridgeConfig};
use hezarfen_backend::constant::{
    AI_ALPN, AI_PODCAST_CANCEL_CAPABILITY, AI_PODCAST_SUBMIT_CAPABILITY, AI_PROTOCOL,
    PODCAST_INTERRUPTED_CODE, PODCAST_JOB_RETENTION_SECS, PODCAST_JOB_STALE_FLOOR_SECS,
};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::module::{Module, ModuleSet};
use hezarfen_backend::tenant::{DEMO_SCHOOL_ID, SchoolId};
use serde_json::{Value, json};
use uuid::Uuid;

const TOKEN: &str = "shared-ai-token";

/// One answer frame, **bounded**: the podcast service's ingest waits on a
/// reporter/uploader handshake, and a bridge that never answers must fail
/// this test by name in seconds rather than hang the run — a hung Test step
/// holds the serialized deploy lock behind it.
async fn frame_or_fail<T: serde::de::DeserializeOwned>(
    recv: &mut quinn::RecvStream,
    what: &str,
) -> T {
    match tokio::time::timeout(std::time::Duration::from_secs(10), read_frame(recv)).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(err)) => panic!("{what}: the frame could not be read: {err}"),
        Err(_) => panic!("{what}: no frame arrived within 10s — the bridge did not answer"),
    }
}


/// The two capabilities a live podcast worker offers. Status and result are
/// not capabilities any more — the backend answers those from its own row — so
/// a service declaring only these serves every reading door.
const CAPABILITIES: [&str; 2] = [AI_PODCAST_SUBMIT_CAPABILITY, AI_PODCAST_CANCEL_CAPABILITY];

/// A source id shaped like the backend ids the service is handed.
const SOURCE_ID: &str = "019732e3-7b00-7000-8000-00000000dead";
/// The narration the caller asks for; the reports echo the same value, as the
/// service's first report resolves it.
const FORMAT: &str = "duz_okuma";
/// The estimate the fake service answers a submit with.
const ETA_SECS: i64 = 2700;
/// What the fake service says its uploaded episode runs for.
const DURATION_SECS: f64 = 12.5;
const AUDIO_NAME: &str = "bolum-1.mp3";
const AUDIO_TYPE: &str = "audio/mpeg";

// ---------------------------------------------------------------- harness --

/// Start a bridge on an ephemeral port.
async fn bridge() -> AiBridge {
    AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: Duration::from_secs(5),
    })
    .await
    .expect("bridge binds on an ephemeral port")
}

/// A QUIC client endpoint that trusts exactly the bridge's own certificate —
/// the same pinning a real service does, rather than a verification bypass.
fn client_endpoint(bridge: &AiBridge) -> quinn::Endpoint {
    hezarfen_backend::ai::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(bridge.certificate()).expect("pin the leaf");
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];

    let quic_tls =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("client TLS is QUIC-usable");
    let mut config = quinn::ClientConfig::new(Arc::new(quic_tls));
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(64u32.into());
    config.transport_config(Arc::new(transport));

    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
    endpoint.set_default_client_config(config);
    endpoint
}

fn demo() -> SchoolId {
    SchoolId::try_parse(DEMO_SCHOOL_ID).expect("the demo slug")
}

fn hello(service: &str, capabilities: &[&str]) -> Hello {
    Hello {
        protocol: AI_PROTOCOL.to_string(),
        service: service.to_string(),
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        token: TOKEN.to_string(),
        max_concurrent: None,
    }
}

/// A connected fake service. Holding it keeps the connection (and so the
/// registration) alive; dropping it is how a test simulates a crash. `conn` is
/// the registration connection itself — the one a real service opens its own
/// streams on when it reports a transition or uploads an episode.
struct FakeService {
    _endpoint: quinn::Endpoint,
    _control: (quinn::SendStream, quinn::RecvStream),
    conn: quinn::Connection,
    /// Every request this service received, in arrival order.
    seen: Arc<Mutex<Vec<Request>>>,
}

impl FakeService {
    fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

/// What the fake service does with each request it receives.
#[derive(Clone)]
enum Behaviour {
    /// Answer like the podcast service: a receipt echoing the backend's own
    /// job id, and a cancel verdict.
    Podcast,
    /// Accept the job under an id of the service's own choosing — the one
    /// answer the submit door must not trust.
    WrongEcho,
    /// Answer with a handled failure.
    Fail { code: String, message: String },
}

/// Dial, register, and start serving requests with `behaviour`.
async fn connect_service(bridge: &AiBridge, hello: Hello, behaviour: Behaviour) -> FakeService {
    let endpoint = client_endpoint(bridge);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .expect("dial")
        .await
        .expect("QUIC handshake");
    let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
    write_frame(&mut send, &hello).await.expect("send Hello");
    let greeting: Greeting = frame_or_fail(&mut recv, "read Greeting").await;
    match greeting {
        Greeting::Welcome { protocol, .. } => assert_eq!(protocol, AI_PROTOCOL),
        other => panic!("expected a welcome, got {other:?}"),
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    serve(conn.clone(), behaviour, Arc::clone(&seen));
    FakeService {
        _endpoint: endpoint,
        _control: (send, recv),
        conn,
        seen,
    }
}

/// The service's request loop: one task per incoming server-initiated stream.
fn serve(conn: quinn::Connection, behaviour: Behaviour, seen: Arc<Mutex<Vec<Request>>>) {
    tokio::spawn(async move {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let behaviour = behaviour.clone();
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let Ok(request) = read_frame::<_, Request>(&mut recv).await else {
                    return;
                };
                seen.lock().unwrap().push(request.clone());
                let response = match behaviour {
                    Behaviour::Podcast => Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload: podcast_answer(&request),
                    },
                    Behaviour::WrongEcho => Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload: wrong_echo_answer(),
                    },
                    Behaviour::Fail { code, message } => Response::Err {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        code,
                        message,
                    },
                };
                let _ = write_frame(&mut send, &response).await;
                let _ = send.finish();
                let _ = send.stopped().await;
            });
        }
    });
}

/// What the podcast service would answer for this capability.
fn podcast_answer(request: &Request) -> Value {
    match request.capability.as_str() {
        // The receipt must echo the backend's own job id — the id every later
        // call names — so the fake echoes the payload it was handed.
        AI_PODCAST_SUBMIT_CAPABILITY => json!({
            "job_id": request.payload["job_id"],
            "state": "queued",
            "eta_secs": ETA_SECS,
        }),
        AI_PODCAST_CANCEL_CAPABILITY => json!({
            "job_id": request.payload["job_id"],
            "cancelled": true,
        }),
        other => panic!("the fake podcast service was asked for `{other}`"),
    }
}

/// The receipt of a service that keyed the job by an id it minted itself.
fn wrong_echo_answer() -> Value {
    json!({ "job_id": "019732e3-7b00-7000-8000-00000000beef", "state": "queued", "eta_secs": 60 })
}

/// Registration completes asynchronously after the welcome is on the wire, so
/// tests that assert on the registry wait for it rather than racing it.
async fn await_workers(bridge: &AiBridge, expected: usize) {
    for _ in 0..300 {
        if bridge.workers().len() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "expected {expected} workers, registry holds {:?}",
        bridge.workers()
    );
}

/// A registered podcast service offering the two capabilities a live worker
/// needs, the router wired to its bridge, a logged-in student's cookie, and
/// the school's database handle — the fixture every round-trip test starts
/// from.
async fn podcast_app() -> (FakeService, Router, String, Database) {
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("podcast", &CAPABILITIES),
        Behaviour::Podcast,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge)).await;
    let cookie = common::login(&app, "ali").await;
    (service, app, cookie, db)
}

/// A logged-in caller and the school's database, with **no AI service in the
/// picture at all**: the reading doors answer from the row, so the lifetime
/// rules and the read refusals need no worker — and a dispatch would be
/// impossible.
async fn app_without_ai() -> (Router, String, Database) {
    let (app, db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    (app, cookie, db)
}

// ------------------------------------------------------------- http doors --

/// Submit one job and return the response.
async fn submit(app: &Router, cookie: &str, body: Value) -> common::Res {
    common::send(app, "POST", "/podcast/jobs", Some(cookie), Some(body)).await
}

/// Submit one job for `source` with the standard format (asserting the `202`)
/// and hand back the backend-minted id every other door names. `source` is the
/// note id: the door resolves the note's own newest PDF before it writes
/// anything, so every submitting test needs a real note behind its source.
async fn submit_job(app: &Router, cookie: &str, source: &str) -> String {
    let res = submit(
        app,
        cookie,
        json!({ "source_id": source, "format": FORMAT }),
    )
    .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    res.body["job_id"]
        .as_str()
        .expect("the receipt names the job")
        .to_string()
}

/// The source a submit narrates, minted through the real doors: a teacher's
/// catalog course, one course note under it, and the note's PDF attachment.
/// Returns (note id, blob key) — the key is what the door hands the service as
/// `source_key`.
async fn note_with_pdf(app: &Router, db: &Database) -> (String, String) {
    let teacher = common::login_as(app, db, "hoca", "teacher").await;
    let course = common::create_course(app, &teacher, "Matematik").await;
    let note = create_note(app, &teacher, &course).await;
    let key = upload_attachment(app, &teacher, &note, "recap.pdf", "application/pdf").await;
    (note, key)
}

/// One course note under `course`, created by `teacher`.
async fn create_note(app: &Router, teacher: &str, course: &str) -> String {
    let res = common::send(
        app,
        "POST",
        "/course-notes",
        Some(teacher),
        Some(json!({ "course": course, "title": "Bölüm 1", "content": "özet" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    common::id_of(&res.body)
}

/// One attachment uploaded onto `note` (asserting the `201`); returns the
/// row's own id — the blob key the bytes are stored under in the school's blob
/// directory.
async fn upload_attachment(
    app: &Router,
    teacher: &str,
    note: &str,
    name: &str,
    content_type: &str,
) -> String {
    let res =
        common::upload_course_note_file(app, teacher, note, name, content_type, b"%PDF-1.4 minimal")
            .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    common::id_of(&res.body)
}

/// One `GET /podcast/jobs/{id}`.
async fn status_of(app: &Router, cookie: &str, id: &str) -> common::Res {
    common::send(app, "GET", &format!("/podcast/jobs/{id}"), Some(cookie), None).await
}

/// One `GET /podcast/jobs/{id}/result`.
async fn result_of(app: &Router, cookie: &str, id: &str) -> common::Res {
    common::send(
        app,
        "GET",
        &format!("/podcast/jobs/{id}/result"),
        Some(cookie),
        None,
    )
    .await
}

/// One `POST /podcast/jobs/{id}/cancel`.
async fn cancel_of(app: &Router, cookie: &str, id: &str) -> common::Res {
    common::send(
        app,
        "POST",
        &format!("/podcast/jobs/{id}/cancel"),
        Some(cookie),
        None,
    )
    .await
}

/// One `GET /podcast/jobs/{id}/audio`, raw — the body is audio bytes or a JSON
/// refusal, never a parsed envelope.
async fn audio_of(app: &Router, cookie: &str, id: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    common::send_raw(
        app,
        "GET",
        &format!("/podcast/jobs/{id}/audio"),
        Some(cookie),
        None,
        Vec::new(),
    )
    .await
}

// ------------------------------------------------------------ service i/o --

/// One `podcast.report` call, exactly as the real service writes it: a
/// client-initiated capability frame on the connection it registered with,
/// echoing the job's own identity — the backend refuses a report that
/// describes a different job.
fn report_frame(
    job_id: &str,
    source_id: &str,
    user_id: &str,
    state: &str,
    stage: &str,
    progress: f64,
) -> Value {
    json!({
        "id": format!("report-{state}"),
        "school": DEMO_SCHOOL_ID,
        "capability": "podcast.report",
        "payload": {
            "job_id": job_id,
            "source_id": source_id,
            "format": FORMAT,
            "user_id": user_id,
            "state": state,
            "stage": stage,
            "progress": progress,
            "error_code": null,
        },
    })
}

/// One `BlobUploadRequest` for `job_id`, declaring the header of the bytes
/// that follow it on the same stream.
fn upload_frame(job_id: &str, name: &str, content_type: &str, size: usize) -> Value {
    json!({
        "id": format!("upload-{job_id}"),
        "upload": true,
        "school": DEMO_SCHOOL_ID,
        "job_id": job_id,
        "name": name,
        "content_type": content_type,
        "size": size,
        "duration_secs": DURATION_SECS,
    })
}

/// Send one client-initiated frame and read the one frame the backend answers
/// with — the shape every service→backend call rides.
async fn capability_call(conn: &quinn::Connection, request: Value) -> Value {
    let (mut send, mut recv) = conn.open_bi().await.expect("client-initiated stream");
    write_frame(&mut send, &request).await.expect("write the call");
    let _ = send.finish();
    frame_or_fail::<Value>(&mut recv, "read the answer").await
}

/// One audio upload on a fresh client-initiated stream: the header frame,
/// exactly `body.len()` raw bytes, FIN — then the backend's one answer frame.
async fn blob_upload(conn: &quinn::Connection, frame: Value, body: &[u8]) -> Value {
    let (mut send, mut recv) = conn.open_bi().await.expect("upload stream");
    write_frame(&mut send, &frame)
        .await
        .expect("write the upload frame");
    send.write_all(body).await.expect("write the audio bytes");
    let _ = send.finish();
    frame_or_fail::<Value>(&mut recv, "read the upload answer").await
}

/// Assert one report was stored: the answer's payload echoes the job and says
/// `stored` — an unstored report is a refusal, never a quiet success.
fn assert_report_stored(answer: &Value, job_id: &str) {
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(answer["payload"]["job_id"], job_id);
    assert_eq!(answer["payload"]["stored"], true);
}

/// The flat refusal code out of one capability answer frame.
fn refusal_code(answer: &Value) -> String {
    assert_eq!(answer["status"], "err", "{answer}");
    answer["code"]
        .as_str()
        .expect("a refusal carries its code")
        .to_string()
}

// ------------------------------------------------------------------- rows --

fn uuid_of(key: &str) -> Uuid {
    Uuid::parse_str(key).expect("a uuid key")
}

/// The wall clock in unix milliseconds — the unit every timestamp column here
/// stores.
fn now_ms() -> i64 {
    Timestamp::now().as_millis()
}

/// How many podcast jobs this user has on the books.
async fn rows_for(db: &Database, user: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM podcast_job WHERE user_id = $1")
        .bind(uuid_of(user))
        .fetch_one(db)
        .await
        .expect("count the user's jobs")
}

/// Write one job row by hand. The shapes the reading doors' own lifetime rules
/// are about — untouched past the staleness window, aged out of retention —
/// are ones no door can mint, so the fixture writes them directly.
async fn seed_job(
    db: &Database,
    job: Uuid,
    user: &str,
    state: &str,
    eta_secs: Option<i64>,
    updated_at: i64,
) -> Uuid {
    sqlx::query(
        "INSERT INTO podcast_job (id, user_id, source_id, state, stage, progress, \
             eta_secs, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, '', 0, $5, $6, $6)",
    )
    .bind(job)
    .bind(uuid_of(user))
    .bind(SOURCE_ID)
    .bind(state)
    .bind(eta_secs)
    .bind(updated_at)
    .execute(db)
    .await
    .expect("seed the job row");
    job
}

/// The same, but `done` and naming an artifact: the row shape the audio door
/// reads its key and headers from, whether or not the bytes are still on disk.
async fn seed_done_job(
    db: &Database,
    job: Uuid,
    user: &str,
    key: &str,
    bytes: i64,
    updated_at: i64,
) -> Uuid {
    sqlx::query(
        "INSERT INTO podcast_job (id, user_id, source_id, state, stage, progress, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, \
             created_at, updated_at) \
         VALUES ($1, $2, $3, 'done', '', 1, $4, $5, $6, $7, $8, $9, $9)",
    )
    .bind(job)
    .bind(uuid_of(user))
    .bind(SOURCE_ID)
    .bind(key)
    .bind(AUDIO_NAME)
    .bind(AUDIO_TYPE)
    .bind(bytes)
    .bind(DURATION_SECS)
    .bind(updated_at)
    .execute(db)
    .await
    .expect("seed the done row");
    job
}

/// One job row in the shape the history reads: a chosen `state`, the note it
/// narrates, and a `created_at` the test owns — the list orders by that
/// column, so a wall-clock stamp would race the assertion. `done` rows name
/// an artifact (the schema's own rule); anything else carries none.
async fn seed_listed_job(
    db: &Database,
    job: Uuid,
    user: &str,
    source: &str,
    state: &str,
    created_at: i64,
) -> Uuid {
    let done = state == "done";
    let audio_key = done.then(|| format!("podcast/{job}.mp3"));
    sqlx::query(
        "INSERT INTO podcast_job (id, user_id, source_id, format, state, stage, progress, \
             error_code, audio_key, audio_name, audio_type, audio_bytes, duration_secs, \
             created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, '', 1, $6, $7, $8, $9, $10, $11, $12, $12)",
    )
    .bind(job)
    .bind(uuid_of(user))
    .bind(source)
    .bind(FORMAT)
    .bind(state)
    .bind((state == "failed").then_some(PODCAST_INTERRUPTED_CODE))
    .bind(&audio_key)
    .bind(done.then_some(AUDIO_NAME))
    .bind(done.then_some(AUDIO_TYPE))
    .bind(done.then_some(4096_i64))
    .bind(done.then_some(DURATION_SECS))
    .bind(created_at)
    .execute(db)
    .await
    .expect("seed the listed job");
    job
}

/// Write `bytes` at `key` under the demo school's blob directory — the layout
/// an ingested episode lands in, and the only place the audio door reads.
fn write_blob(key: &str, bytes: &[u8]) {
    let path = common::blob_dir().join(key);
    std::fs::create_dir_all(path.parent().expect("parent dir")).expect("create the blob tree");
    std::fs::write(&path, bytes).expect("write the blob");
    assert!(path.is_file(), "the fixture blob really landed");
}

/// Deterministic pseudo-random episode bytes, past one 64 KiB read chunk so a
/// passing door proves it streamed the body rather than fitting one write.
fn episode_bytes() -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..128 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

// ------------------------------------------------------------ round trips --

/// The submit door writes the row before it dispatches, and hands the service
/// the id it minted — the one handle every later call names. A service keying
/// its own record by what it was sent can therefore never disagree with the
/// row, and the receipt carries the service's own ETA.
#[tokio::test]
async fn a_submit_writes_the_row_and_hands_the_service_its_own_job_id() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, key) = note_with_pdf(&app, &db).await;

    let res = submit(
        &app,
        &cookie,
        json!({ "source_id": &note, "format": FORMAT }),
    )
    .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(res.body["state"], "queued");
    assert_eq!(res.body["eta_secs"], ETA_SECS);
    let job_id = res.body["job_id"]
        .as_str()
        .expect("the receipt names the job")
        .to_string();

    let seen = service.seen();
    assert_eq!(seen.len(), 1, "exactly one dispatch");
    assert_eq!(seen[0].capability, AI_PODCAST_SUBMIT_CAPABILITY);
    assert_eq!(seen[0].school, DEMO_SCHOOL_ID, "the school rides the frame");
    assert_eq!(
        seen[0].payload["job_id"], job_id,
        "the service was handed the row's own id"
    );
    assert_eq!(seen[0].payload["source_id"], note);
    assert_eq!(
        seen[0].payload["source_key"], key,
        "the note's PDF blob key rides the dispatch: {}",
        seen[0].payload
    );
    assert_eq!(seen[0].payload["format"], FORMAT);
    assert_eq!(seen[0].payload["user_id"], user);

    let (state, eta, source): (String, Option<i64>, String) =
        sqlx::query_as("SELECT state, eta_secs, source_id FROM podcast_job WHERE id = $1")
            .bind(uuid_of(&job_id))
            .fetch_one(&db)
            .await
            .expect("the row is on the books");
    assert_eq!(state, "queued");
    assert_eq!(eta, Some(ETA_SECS), "the service's estimate landed on the row");
    assert_eq!(source, note, "the row records the note the job narrates");
}

/// A service that accepts the job under an id of its own has answered about a
/// different job: the echo is checked, not trusted, because the backend's id is
/// the only handle every later call uses. The refusal is a `502` — the backend
/// cannot vouch for an answer it did not understand — and the row is
/// tombstoned rather than left claiming `queued` forever.
#[tokio::test]
async fn a_submit_that_echoes_another_job_id_is_a_bad_gateway() {
    let bridge = bridge().await;
    let _service =
        connect_service(&bridge, hello("podcast", &CAPABILITIES), Behaviour::WrongEcho).await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge)).await;
    let cookie = common::login(&app, "ali").await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;

    let res = submit(&app, &cookie, json!({ "source_id": note })).await;
    assert_eq!(res.status, StatusCode::BAD_GATEWAY, "{}", res.body);
    assert_eq!(res.body["error"], "bad_reply");

    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT state, error_code FROM podcast_job WHERE user_id = $1")
            .bind(uuid_of(&user))
            .fetch_all(&db)
            .await
            .expect("read back the tombstone");
    assert_eq!(
        rows,
        vec![("failed".to_string(), Some("bad_reply".to_string()))],
        "exactly one row, and it records why"
    );
}

/// The format is optional and the service applies its own default: an omitted
/// format must not appear on the wire at all, or the service would have to
/// distinguish "absent" from "empty".
#[tokio::test]
async fn an_omitted_format_is_not_sent_at_all() {
    let (service, app, cookie, db) = podcast_app().await;
    let (note, _key) = note_with_pdf(&app, &db).await;

    let res = submit(&app, &cookie, json!({ "source_id": note })).await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);

    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].payload.get("format").is_none(),
        "no format was asked for, so none is sent: {}",
        seen[0].payload
    );
}

/// A blank source id is refused here, before the bridge and before the row:
/// the service would refuse it too, and a round trip to learn that is a round
/// trip wasted.
#[tokio::test]
async fn a_blank_source_id_is_refused_before_anything_is_written() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;

    let res = submit(&app, &cookie, json!({ "source_id": "   " })).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(service.seen().is_empty(), "nothing reaches the service");
    assert_eq!(rows_for(&db, &user).await, 0, "and no row is written");
}

/// A report lands on the row, and the reading doors answer from it: the
/// service's `seen` log must not grow, because `podcast.status` and
/// `podcast.result` are not calls the backend makes any more — a relayed read
/// would show up here as a second request.
#[tokio::test]
async fn a_running_report_lands_on_the_row_and_the_reads_stay_off_the_service() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &cookie, &note).await;

    assert_report_stored(
        &capability_call(
            &service.conn,
            report_frame(&job_id, &note, &user, "running", "tts", 0.5),
        )
        .await,
        &job_id,
    );

    let res = status_of(&app, &cookie, &job_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], job_id);
    assert_eq!(res.body["state"], "running");
    assert_eq!(res.body["stage"], "tts");
    assert_eq!(res.body["progress"], 0.5);
    assert!(res.body["error_code"].is_null());

    let state: String = sqlx::query_scalar("SELECT state FROM podcast_job WHERE id = $1")
        .bind(uuid_of(&job_id))
        .fetch_one(&db)
        .await
        .expect("the report really landed");
    assert_eq!(state, "running");
    assert_eq!(
        service.seen().len(),
        1,
        "the reads answered from the row, not the service"
    );
}

/// The whole pipeline, end to end: submit, a running report, the finished mp3
/// uploaded as raw bytes, the done report, then the result door naming the
/// artifact and the audio door streaming the very bytes that were uploaded —
/// with the content type the row recorded.
#[tokio::test]
async fn a_finished_job_serves_the_uploaded_bytes() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &cookie, &note).await;

    assert_report_stored(
        &capability_call(
            &service.conn,
            report_frame(&job_id, &note, &user, "running", "tts", 0.25),
        )
        .await,
        &job_id,
    );

    let bytes = episode_bytes();
    let answer = blob_upload(
        &service.conn,
        upload_frame(&job_id, AUDIO_NAME, AUDIO_TYPE, bytes.len()),
        &bytes,
    )
    .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(answer["key"], format!("podcast/{job_id}.mp3"));
    assert_eq!(answer["size"], bytes.len() as u64);

    assert_report_stored(
        &capability_call(
            &service.conn,
            report_frame(&job_id, &note, &user, "done", "", 1.0),
        )
        .await,
        &job_id,
    );

    let res = result_of(&app, &cookie, &job_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], job_id);
    assert_eq!(res.body["audio_id"], format!("podcast/{job_id}.mp3"));
    assert_eq!(res.body["duration_secs"], DURATION_SECS);
    assert_eq!(res.body["format"], FORMAT);

    let (status, headers, body) = audio_of(&app, &cookie, &job_id).await;
    assert_eq!(status, StatusCode::OK, "{:?}", String::from_utf8_lossy(&body));
    assert_eq!(headers["content-type"], AUDIO_TYPE, "the row's own type");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(body, bytes, "the door streams the uploaded bytes, unchanged");

    assert_eq!(
        service.seen().len(),
        1,
        "the upload and the reports are the service's own calls, not dispatches"
    );
}

/// Cancelling a live job reaches the worker that owns the queue and stamps the
/// verdict on the row — the row then records that the job stopped, so a poll
/// after the service is gone still reads the truth.
#[tokio::test]
async fn a_cancel_of_a_live_job_stops_it() {
    let (service, app, cookie, db) = podcast_app().await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &cookie, &note).await;

    let res = cancel_of(&app, &cookie, &job_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], job_id);
    assert_eq!(res.body["cancelled"], true);

    let seen = service.seen();
    assert_eq!(seen.len(), 2, "the cancel rode a second dispatch");
    assert_eq!(seen[1].capability, AI_PODCAST_CANCEL_CAPABILITY);
    assert_eq!(seen[1].payload["job_id"], job_id);

    let state: String = sqlx::query_scalar("SELECT state FROM podcast_job WHERE id = $1")
        .bind(uuid_of(&job_id))
        .fetch_one(&db)
        .await
        .expect("the row records the verdict");
    assert_eq!(state, "cancelled");
}

/// A job that already stopped is not an error to cancel: the verdict is
/// `false` — this call cancelled nothing — and no worker is needed at all,
/// because the row is terminal and the door answers before it looks for one.
#[tokio::test]
async fn a_cancel_of_a_terminal_job_answers_false_without_the_service() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &cookie, &note).await;
    assert_report_stored(
        &capability_call(
            &service.conn,
            report_frame(&job_id, &note, &user, "failed", "", 0.0),
        )
        .await,
        &job_id,
    );

    // The pipeline is gone by the time the caller gives up on it.
    drop(service);

    let res = cancel_of(&app, &cookie, &job_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], job_id);
    assert_eq!(res.body["cancelled"], false);

    let state: String = sqlx::query_scalar("SELECT state FROM podcast_job WHERE id = $1")
        .bind(uuid_of(&job_id))
        .fetch_one(&db)
        .await
        .expect("the row is still there");
    assert_eq!(state, "failed", "the verdict never rewrites a terminal row");
}

// ------------------------------------------------------------- the history --

/// One `GET /podcast/jobs…` — the caller's own history.
async fn list_of(app: &Router, cookie: &str, query: &str) -> common::Res {
    common::send(
        app,
        "GET",
        &format!("/podcast/jobs{query}"),
        Some(cookie),
        None,
    )
    .await
}

/// The `job_id`s of one list response, in the order it returned them.
fn ids_of(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("a list response carries items")
        .iter()
        .map(|item| {
            item["job_id"]
                .as_str()
                .expect("every item names its job")
                .to_string()
        })
        .collect()
}

/// The history shows what the school produced: a finished episode comes back
/// with the title of the note it narrates, its running time, and where it
/// ended — all from the backend's own row, with no service in the picture.
#[tokio::test]
async fn the_history_lists_a_finished_job_with_its_source_title() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let stamp = now_ms();
    let job = seed_listed_job(&db, Uuid::now_v7(), &user, &note, "done", stamp).await;

    let res = list_of(&app, &cookie, "").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["total"], 1);
    assert_eq!(res.body["offset"], 0);
    let item = &res.body["items"][0];
    assert_eq!(item["job_id"], job.to_string());
    assert_eq!(item["state"], "done");
    assert_eq!(item["format"], FORMAT);
    assert_eq!(item["source_id"], note);
    assert_eq!(item["source_title"], "Bölüm 1");
    assert_eq!(item["created_at"], stamp);
    assert_eq!(item["finished_at"], stamp);
    assert_eq!(item["duration_secs"], DURATION_SECS);
    assert!(item["error_code"].is_null());
}

/// Newest first, with the id breaking the tie: the frozen order, on rows
/// seeded out of order — including two that share their instant.
#[tokio::test]
async fn the_history_is_newest_first() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let base = now_ms() - 3 * 60 * 60 * 1_000;
    let oldest = seed_listed_job(&db, Uuid::now_v7(), &user, SOURCE_ID, "done", base).await;
    let middle = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        SOURCE_ID,
        "failed",
        base + 3_600_000,
    )
    .await;
    let tie_old = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        SOURCE_ID,
        "done",
        base + 7_200_000,
    )
    .await;
    let tie_new = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        SOURCE_ID,
        "done",
        base + 7_200_000,
    )
    .await;

    let res = list_of(&app, &cookie, "").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["total"], 4);
    assert_eq!(
        ids_of(&res.body),
        vec![
            tie_new.to_string(),
            tie_old.to_string(),
            middle.to_string(),
            oldest.to_string()
        ]
    );
}

/// `?limit=&offset=` slice the ordered history and `total` stays the unpaged
/// count, so a client can page without a second request — and an offset past
/// the tail is an empty page, not an error.
#[tokio::test]
async fn the_history_pages_with_limit_and_offset() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let base = now_ms() - 3 * 60 * 60 * 1_000;
    let oldest = seed_listed_job(&db, Uuid::now_v7(), &user, SOURCE_ID, "done", base).await;
    let middle = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        SOURCE_ID,
        "done",
        base + 3_600_000,
    )
    .await;
    let newest = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        SOURCE_ID,
        "done",
        base + 7_200_000,
    )
    .await;

    let first = list_of(&app, &cookie, "?limit=2").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["total"], 3);
    assert_eq!(first.body["limit"], 2);
    assert_eq!(
        ids_of(&first.body),
        vec![newest.to_string(), middle.to_string()]
    );

    let second = list_of(&app, &cookie, "?limit=1&offset=1").await;
    assert_eq!(second.body["total"], 3);
    assert_eq!(second.body["offset"], 1);
    assert_eq!(ids_of(&second.body), vec![middle.to_string()]);

    let past = list_of(&app, &cookie, "?limit=2&offset=9").await;
    assert_eq!(past.body["total"], 3);
    assert!(ids_of(&past.body).is_empty());

    let bad = list_of(&app, &cookie, "?limit=0").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    assert!(
        bad.body["error"]
            .as_str()
            .expect("an error")
            .contains("limit"),
        "the refusal names the field: {}",
        bad.body
    );

    let unpaged = list_of(&app, &cookie, "?offset=1").await;
    assert_eq!(unpaged.body["total"], 3);
    assert!(unpaged.body["limit"].is_null());
    assert_eq!(
        ids_of(&unpaged.body).len(),
        2,
        "{oldest} is the one skipped"
    );
}

/// A school with no episodes yet answers an empty page — `items: []`,
/// `total: 0` — never a `404`.
#[tokio::test]
async fn an_empty_history_is_an_empty_page() {
    let (app, cookie, _db) = app_without_ai().await;

    let res = list_of(&app, &cookie, "").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["total"], 0);
    assert!(ids_of(&res.body).is_empty());
    assert!(res.body["limit"].is_null());
}

/// `?source_id=` narrows the history to one note's own episodes — the studio
/// panel's list — and `total` counts that same filtered predicate, so a client
/// can page a scoped list. A note with no episodes is an empty page, not an
/// error; omitting the filter still returns every job, so the absent case is
/// exactly the response it always was.
#[tokio::test]
async fn the_history_scopes_to_one_note_when_asked() {
    let (app, cookie, db) = app_without_ai().await;
    let teacher = common::login_as(&app, &db, "hoca", "teacher").await;
    let course = common::create_course(&app, &teacher, "Matematik").await;
    let first = create_note(&app, &teacher, &course).await;
    let second = create_note(&app, &teacher, &course).await;
    let empty = create_note(&app, &teacher, &course).await;
    let user = common::me_id(&app, &cookie).await;
    let base = now_ms() - 3 * 60 * 60 * 1_000;
    let older = seed_listed_job(&db, Uuid::now_v7(), &user, &first, "done", base).await;
    let newer = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        &first,
        "failed",
        base + 3_600_000,
    )
    .await;
    let other = seed_listed_job(
        &db,
        Uuid::now_v7(),
        &user,
        &second,
        "done",
        base + 7_200_000,
    )
    .await;

    let scoped = list_of(&app, &cookie, &format!("?source_id={first}")).await;
    assert_eq!(scoped.status, StatusCode::OK, "{}", scoped.body);
    assert_eq!(scoped.body["total"], 2, "the count is the filtered count");
    assert_eq!(
        ids_of(&scoped.body),
        vec![newer.to_string(), older.to_string()],
        "newest first, and only the asked note's rows"
    );
    assert!(
        scoped.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|item| item["source_id"] == first),
        "every row narrates the asked note: {}",
        scoped.body
    );

    let second_only = list_of(&app, &cookie, &format!("?source_id={second}")).await;
    assert_eq!(second_only.status, StatusCode::OK, "{}", second_only.body);
    assert_eq!(second_only.body["total"], 1);
    assert_eq!(ids_of(&second_only.body), vec![other.to_string()]);

    let none = list_of(&app, &cookie, &format!("?source_id={empty}")).await;
    assert_eq!(none.status, StatusCode::OK, "{}", none.body);
    assert_eq!(none.body["total"], 0, "a note with no episodes is an empty page");
    assert!(ids_of(&none.body).is_empty());

    let all = list_of(&app, &cookie, "").await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
    assert_eq!(all.body["total"], 3, "omitting the filter returns every job");
    assert_eq!(ids_of(&all.body).len(), 3);
}

/// A `source_id` that is not a uuid is a `400` naming the field — refused
/// rather than parsed to the nil id and answering an empty page that hides the
/// caller's typo.
#[tokio::test]
async fn a_malformed_source_filter_is_refused() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let _ = seed_listed_job(&db, Uuid::now_v7(), &user, SOURCE_ID, "done", now_ms()).await;

    for bad in ["not-a-uuid", "%20", ""] {
        let res = list_of(&app, &cookie, &format!("?source_id={bad}")).await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "source_id={bad:?}: {}",
            res.body
        );
        assert!(
            res.body["error"]
                .as_str()
                .expect("an error")
                .contains("source_id"),
            "the refusal names the field: {}",
            res.body
        );
    }
}

/// A live job lists as itself, with everything only a finished episode can
/// know left `null` — and a job nobody has updated inside its ETA-scaled
/// window reads `failed`/`interrupted`, the same read-side projection the
/// per-id door applies. The projection writes nothing: the row still says
/// `running` afterwards.
#[tokio::test]
async fn a_live_job_lists_with_nulls_and_a_stale_one_reads_failed() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let fresh = seed_listed_job(&db, Uuid::now_v7(), &user, SOURCE_ID, "running", now_ms()).await;
    let aged = now_ms() - (PODCAST_JOB_STALE_FLOOR_SECS + 100) * 1_000;
    let stale = seed_listed_job(&db, Uuid::now_v7(), &user, SOURCE_ID, "running", aged).await;

    let res = list_of(&app, &cookie, "").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let items = res.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    let live = items
        .iter()
        .find(|item| item["job_id"] == fresh.to_string())
        .expect("the fresh job is listed");
    assert_eq!(live["state"], "running");
    assert!(live["finished_at"].is_null());
    assert!(live["duration_secs"].is_null());
    assert!(live["error_code"].is_null());
    assert!(live["source_title"].is_null(), "no note answers to that id");
    let dead = items
        .iter()
        .find(|item| item["job_id"] == stale.to_string())
        .expect("the stale job is listed");
    assert_eq!(dead["state"], "failed");
    assert_eq!(dead["error_code"], "interrupted");

    let stored: String = sqlx::query_scalar("SELECT state FROM podcast_job WHERE id = $1")
        .bind(stale)
        .fetch_one(&db)
        .await
        .expect("the row is still there");
    assert_eq!(stored, "running", "the projection wrote nothing");
}

/// The history is school-scoped like every other door: each school lists its
/// own episodes only, even when both hold a job for a user of the same name —
/// and a note's title never crosses either.
#[tokio::test]
async fn the_history_never_shows_another_schools_jobs() {
    let deployment = common::deployment_with(&[("beta", "Beta Koleji")]).await;
    let app = &deployment.app;
    let demo_db = common::demo_db(&deployment.tenants).await;
    let beta_db = deployment
        .tenants
        .get(&SchoolId::try_parse(hezarfen_backend::tenant::BETA_SCHOOL_ID).expect("the beta slug"))
        .await
        .expect("the beta school's handle");
    let demo_cookie = common::login_as_school(app, &demo_db, DEMO_SCHOOL_ID, "ali", "teacher").await;
    let beta_cookie = common::login_as_school(app, &beta_db, "beta", "ali", "teacher").await;
    let demo_user = common::me_id(app, &demo_cookie).await;
    let beta_user = common::me_id(app, &beta_cookie).await;

    let course = common::create_course(app, &demo_cookie, "Matematik").await;
    let demo_note = create_note(app, &demo_cookie, &course).await;
    let demo_job = seed_listed_job(
        &demo_db,
        Uuid::now_v7(),
        &demo_user,
        &demo_note,
        "done",
        now_ms(),
    )
    .await;
    let beta_job = seed_listed_job(
        &beta_db,
        Uuid::now_v7(),
        &beta_user,
        SOURCE_ID,
        "done",
        now_ms(),
    )
    .await;

    let demo_list = list_of(app, &demo_cookie, "").await;
    assert_eq!(demo_list.status, StatusCode::OK, "{}", demo_list.body);
    assert_eq!(demo_list.body["total"], 1);
    assert_eq!(ids_of(&demo_list.body), vec![demo_job.to_string()]);
    assert_eq!(demo_list.body["items"][0]["source_title"], "Bölüm 1");

    let beta_list = list_of(app, &beta_cookie, "").await;
    assert_eq!(beta_list.status, StatusCode::OK, "{}", beta_list.body);
    assert_eq!(beta_list.body["total"], 1);
    assert_eq!(ids_of(&beta_list.body), vec![beta_job.to_string()]);
    assert_ne!(beta_list.body["items"][0]["job_id"], demo_job.to_string());

    // The filter is school-scoped like the list itself: the demo school's own
    // note scopes to its one job, and the other school's note id names no job
    // here, whichever side asks — a foreign note id is an empty page, not a
    // window into the other school.
    let demo_scoped = list_of(app, &demo_cookie, &format!("?source_id={demo_note}")).await;
    assert_eq!(demo_scoped.status, StatusCode::OK, "{}", demo_scoped.body);
    assert_eq!(demo_scoped.body["total"], 1);
    assert_eq!(ids_of(&demo_scoped.body), vec![demo_job.to_string()]);

    let cross = list_of(app, &demo_cookie, &format!("?source_id={SOURCE_ID}")).await;
    assert_eq!(cross.status, StatusCode::OK, "{}", cross.body);
    assert_eq!(cross.body["total"], 0, "beta's note names no demo job");
    assert!(ids_of(&cross.body).is_empty());

    let cross_back = list_of(app, &beta_cookie, &format!("?source_id={demo_note}")).await;
    assert_eq!(cross_back.status, StatusCode::OK, "{}", cross_back.body);
    assert_eq!(cross_back.body["total"], 0, "demo's note names no beta job");
    assert!(ids_of(&cross_back.body).is_empty());
}

// --------------------------------------------------------------- refusals --

/// A submit that reaches no service writes nothing: the `503` is the whole
/// answer, so a poll can never find a job nobody will ever work on. Both
/// shapes of "nobody to ask" answer that `503` with their own message — a
/// configured-but-unconnected bridge, and a deployment with no bridge at all.
#[tokio::test]
async fn a_submit_that_reaches_no_service_is_503_and_writes_no_row() {
    let (app, db) = common::app_with_ai(Some(bridge().await)).await;
    let cookie = common::login(&app, "ali").await;
    let user = common::me_id(&app, &cookie).await;
    let res = submit(&app, &cookie, json!({ "source_id": SOURCE_ID })).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
    assert_eq!(res.body["error"], "no AI service is connected right now");
    assert_eq!(rows_for(&db, &user).await, 0, "no worker, no row");

    // The deployment never set `AI_QUIC_ADDR`: there is no bridge at all.
    let (app, db) = common::app_with_ai(None).await;
    let cookie = common::login(&app, "ali").await;
    let user = common::me_id(&app, &cookie).await;
    let res = submit(&app, &cookie, json!({ "source_id": SOURCE_ID })).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
    assert_eq!(
        res.body["error"],
        "the AI service is not enabled on this deployment"
    );
    assert_eq!(rows_for(&db, &user).await, 0);
}

/// A service that refuses the job — at capacity, say — leaves the row as the
/// record of that: exactly one row, `failed` with the service's own code, and
/// the refusal reaches the caller with its own status.
#[tokio::test]
async fn a_refusing_service_tombstones_the_row() {
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("podcast", &CAPABILITIES),
        Behaviour::Fail {
            code: "busy".to_string(),
            message: "the queue is at capacity".to_string(),
        },
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge)).await;
    let cookie = common::login(&app, "ali").await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;

    let res = submit(&app, &cookie, json!({ "source_id": note })).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
    assert_eq!(res.body["error"], "busy");

    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT state, error_code FROM podcast_job WHERE user_id = $1")
            .bind(uuid_of(&user))
            .fetch_all(&db)
            .await
            .expect("read back the tombstone");
    assert_eq!(
        rows,
        vec![("failed".to_string(), Some("busy".to_string()))],
        "exactly one row, and it records the refusal"
    );
}

/// A foreign job id reads as absent on every door — the same `404` an id that
/// never existed earns, so the answer leaks nothing about somebody else's job.
/// The cancel is the sharp one: it must not reach the service on a foreign id
/// either.
#[tokio::test]
async fn another_users_job_reads_as_absent() {
    let (service, app, ali, db) = podcast_app().await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &ali, &note).await;
    let seen_before = service.seen().len();

    let ayse = common::login(&app, "ayse").await;
    let doors = [
        ("GET", format!("/podcast/jobs/{job_id}")),
        ("GET", format!("/podcast/jobs/{job_id}/result")),
        ("GET", format!("/podcast/jobs/{job_id}/audio")),
        ("POST", format!("/podcast/jobs/{job_id}/cancel")),
    ];
    for (method, uri) in doors {
        let res = common::send(&app, method, &uri, Some(&ayse), None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method} {uri}: {}", res.body);
        assert_eq!(res.body["error"], "not found");
    }
    assert_eq!(
        service.seen().len(),
        seen_before,
        "nothing about a foreign job was dispatched"
    );
}

/// A terminal job past its retention window is gone for good: `410`, not the
/// `404` of a job that never existed — the row is still there, but nothing
/// about it is served any more.
#[tokio::test]
async fn an_expired_job_reads_as_gone() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let job = Uuid::now_v7();
    let key = format!("podcast/{job}.mp3");
    // Terminal, with its artifact named (the schema's `done` CHECK), and one
    // minute past the retention window.
    let aged = now_ms() - (PODCAST_JOB_RETENTION_SECS + 60) * 1_000;
    seed_done_job(&db, job, &user, &key, 41, aged).await;

    let res = status_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(res.status, StatusCode::GONE, "{}", res.body);
    assert_eq!(res.body["error"], "this podcast job has expired");
}

/// A job nobody has updated inside its own ETA-scaled window is presented as
/// `failed`/`interrupted` — and the projection is read-side only: the row the
/// database holds still says `running`, so the service's next report (or its
/// own sweep) is what actually repairs it.
#[tokio::test]
async fn a_stale_job_reads_as_interrupted_but_the_row_is_untouched() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let job = Uuid::now_v7();
    // Past the staleness floor (the window a small ETA resolves to), far
    // inside the retention window.
    let aged = now_ms() - (PODCAST_JOB_STALE_FLOOR_SECS + 100) * 1_000;
    seed_job(&db, job, &user, "running", Some(5), aged).await;

    let res = status_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["state"], "failed");
    assert_eq!(res.body["error_code"], "interrupted");

    let stored: String = sqlx::query_scalar("SELECT state FROM podcast_job WHERE id = $1")
        .bind(job)
        .fetch_one(&db)
        .await
        .expect("the row is still there");
    assert_eq!(stored, "running", "the projection wrote nothing");
}

/// Neither the result nor the bytes exist before the job is done: both reading
/// doors answer `409` with `not_ready`, so a client branches on the code
/// instead of parsing the sentence.
#[tokio::test]
async fn the_result_and_audio_doors_are_not_ready_before_done() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &cookie, &note).await;
    assert_report_stored(
        &capability_call(
            &service.conn,
            report_frame(&job_id, &note, &user, "running", "tts", 0.5),
        )
        .await,
        &job_id,
    );

    let res = result_of(&app, &cookie, &job_id).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "not_ready");

    let (status, _, body) = audio_of(&app, &cookie, &job_id).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "{:?}",
        String::from_utf8_lossy(&body)
    );
    let refusal: Value = serde_json::from_slice(&body).expect("a JSON refusal");
    assert_eq!(refusal["code"], "not_ready");
}

/// A `done` report that arrives before its audio was uploaded is refused
/// `audio_missing`: the row must never claim a finished episode whose bytes are
/// not there (the schema's own CHECK backstops the same rule), and the row
/// stays `running` so the service can upload and report again.
#[tokio::test]
async fn a_done_report_before_the_upload_is_refused() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, _key) = note_with_pdf(&app, &db).await;
    let job_id = submit_job(&app, &cookie, &note).await;
    assert_report_stored(
        &capability_call(
            &service.conn,
            report_frame(&job_id, &note, &user, "running", "tts", 0.5),
        )
        .await,
        &job_id,
    );

    let answer = capability_call(
        &service.conn,
        report_frame(&job_id, &note, &user, "done", "", 1.0),
    )
    .await;
    assert_eq!(refusal_code(&answer), "audio_missing");

    let state: String = sqlx::query_scalar("SELECT state FROM podcast_job WHERE id = $1")
        .bind(uuid_of(&job_id))
        .fetch_one(&db)
        .await
        .expect("the row is still there");
    assert_eq!(state, "running", "the refusal wrote nothing");
}

/// A `done` row whose blob is gone from the host answers `409 audio_missing`:
/// the record is intact and the bytes are not — a state a client can report,
/// unlike a generic `500`. The result door still names the artifact, so the
/// row itself is provably fine and only the file is missing.
#[tokio::test]
async fn the_audio_door_reports_a_missing_blob() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let job = Uuid::now_v7();
    let key = format!("podcast/{job}.mp3");
    seed_done_job(&db, job, &user, &key, 41, now_ms()).await;
    assert!(
        !common::blob_dir().join(&key).exists(),
        "the blob really is gone"
    );

    let res = result_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["audio_id"], key);

    let (status, _, body) = audio_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "{:?}",
        String::from_utf8_lossy(&body)
    );
    let refusal: Value = serde_json::from_slice(&body).expect("a JSON refusal");
    assert_eq!(refusal["code"], "audio_missing");
}

/// The reading doors need no AI service at all: a finished job's row and its
/// blob are served with no bridge configured — which is the whole reason the
/// row exists (a restarted service, or a wiped service volume, costs nothing
/// but the liveness the row's own projection already covers).
#[tokio::test]
async fn the_reading_doors_need_no_service() {
    let (app, cookie, db) = app_without_ai().await;
    let user = common::me_id(&app, &cookie).await;
    let job = Uuid::now_v7();
    let key = format!("podcast/{job}.mp3");
    let bytes = episode_bytes();
    write_blob(&key, &bytes);
    seed_done_job(&db, job, &user, &key, bytes.len() as i64, now_ms()).await;

    let res = status_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["state"], "done");

    let res = result_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["audio_id"], key);

    let (status, headers, body) = audio_of(&app, &cookie, &job.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], AUDIO_TYPE);
    assert_eq!(body, bytes);
}

/// The old `GET /podcast/audio?path=…` door is gone with the caller-chosen path
/// it resolved: it could not be owner-scoped, so the whole route is a `404` —
/// the per-job door is the only audio door, and it streams what the school's
/// own row names. The fixture proves the file exists, so the answer is the
/// missing route, not a missing target.
#[tokio::test]
async fn the_old_path_audio_door_is_gone() {
    let (app, cookie, _db) = app_without_ai().await;
    write_blob("podcast/escape/kept.mp3", b"bytes the old door would have served");

    let res = common::send(
        &app,
        "GET",
        "/podcast/audio?path=podcast/escape/kept.mp3",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// The submit door resolves the source itself: a note with no PDF attachment
/// is refused `409 source_missing` **before** anything is written. Pre-fix the
/// door accepted the job and it died later in the service's `kaynak` stage —
/// a queued job guaranteed to fail, which is what this refusal exists to
/// prevent. A newer non-PDF attachment changes nothing: it is not a source.
#[tokio::test]
async fn a_note_with_no_pdf_is_refused_at_the_door() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let teacher = common::login_as(&app, &db, "hoca", "teacher").await;
    let course = common::create_course(&app, &teacher, "Matematik").await;
    let note = create_note(&app, &teacher, &course).await;

    let res = submit(&app, &cookie, json!({ "source_id": &note })).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "source_missing");

    // A non-PDF attachment is newer than nothing, and is still not a source.
    let _txt = upload_attachment(&app, &teacher, &note, "ozet.txt", "text/plain").await;
    let res = submit(&app, &cookie, json!({ "source_id": &note })).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "source_missing");

    assert!(
        service.seen().is_empty(),
        "the refusal never reaches a worker: {:?}",
        service.seen()
    );
    assert_eq!(rows_for(&db, &user).await, 0, "and no row is written");
}

/// A note whose PDF is on record but whose blob is gone from this host is
/// refused the same way: the row is intact and the bytes are not, and the
/// service could only fail on it — so the door answers now instead of queueing
/// a job nobody can finish.
#[tokio::test]
async fn a_pdf_whose_blob_is_gone_is_refused_at_the_door() {
    let (service, app, cookie, db) = podcast_app().await;
    let user = common::me_id(&app, &cookie).await;
    let (note, key) = note_with_pdf(&app, &db).await;

    let path = common::blob_dir().join(&key);
    assert!(path.is_file(), "the upload wrote the blob");
    std::fs::remove_file(&path).expect("take the blob away");
    assert!(!path.exists(), "the blob really is gone");

    let res = submit(&app, &cookie, json!({ "source_id": note })).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "source_missing");

    assert!(service.seen().is_empty(), "nothing was dispatched");
    assert_eq!(rows_for(&db, &user).await, 0, "and no row is written");
}

/// Which attachment narrates: the note's **newest PDF**, not its newest file —
/// a text recap uploaded after the PDF is not a source, and a corrected PDF
/// uploaded after that is.
#[tokio::test]
async fn the_source_is_the_newest_pdf_attachment() {
    let (service, app, cookie, db) = podcast_app().await;
    let teacher = common::login_as(&app, &db, "hoca", "teacher").await;
    let course = common::create_course(&app, &teacher, "Matematik").await;
    let note = create_note(&app, &teacher, &course).await;

    let draft = upload_attachment(&app, &teacher, &note, "taslak.pdf", "application/pdf").await;
    let _newer_text =
        upload_attachment(&app, &teacher, &note, "ozet.txt", "text/plain").await;
    let corrected =
        upload_attachment(&app, &teacher, &note, "duzeltilmis.pdf", "application/pdf").await;
    assert_ne!(draft, corrected, "two distinct attachment rows");

    let res = submit(&app, &cookie, json!({ "source_id": &note })).await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);

    let seen = service.seen();
    assert_eq!(seen.len(), 1, "exactly one dispatch");
    assert_eq!(seen[0].payload["source_id"], note);
    assert_eq!(
        seen[0].payload["source_key"], corrected,
        "the newest PDF, not the newest file: {}",
        seen[0].payload
    );
}

/// Authentication is the whole gate on every door: no cookie is a `401`, not a
/// handler — and not a dispatch under somebody else's name either.
#[tokio::test]
async fn every_door_requires_a_session() {
    let (service, app, _cookie, _db) = podcast_app().await;
    let id = common::GHOST_ID;

    let doors = [
        ("POST", "/podcast/jobs".to_string(), Some(json!({ "source_id": SOURCE_ID }))),
        ("GET", "/podcast/jobs".to_string(), None),
        ("GET", format!("/podcast/jobs/{id}"), None),
        ("GET", format!("/podcast/jobs/{id}/result"), None),
        ("GET", format!("/podcast/jobs/{id}/audio"), None),
        ("POST", format!("/podcast/jobs/{id}/cancel"), None),
    ];
    for (method, uri, body) in doors {
        let res = common::send(&app, method, &uri, None, body).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri}: {}",
            res.body
        );
        assert_eq!(res.body["error"], "unauthorized");
    }
    assert!(
        service.seen().is_empty(),
        "an anonymous call dispatches nothing"
    );
}

/// The nest sits behind the school's `chatbot` module — the AI package's only
/// module — exactly like `/chatbot` and `/rag`: with the module off the whole
/// URL space answers the disabled refusal, never a handler.
#[tokio::test]
async fn a_school_without_the_chatbot_module_has_no_podcast_nest() {
    let bridge = bridge().await;
    let (app, _db, tenants) = common::app_with_ai_tenants(Some(bridge)).await;
    let cookie = common::login(&app, "ali").await;

    let mut modules = ModuleSet::all();
    modules.remove(Module::Chatbot);
    modules.validate().expect("the narrowed set is satisfiable");
    tenants
        .set_modules(&demo(), &modules)
        .await
        .expect("narrow the demo school");

    let res = status_of(&app, &cookie, common::GHOST_ID).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(res.body["module"], "chatbot");
}
