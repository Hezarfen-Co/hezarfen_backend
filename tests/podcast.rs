//! End-to-end tests for the podcast nest, against a fake podcast service.
//!
//! Everything here runs over a real QUIC socket on loopback — a real
//! handshake, real TLS, real streams — with [`Behaviour::Podcast`] standing in
//! for what the service would do with a request. The four dispatching doors
//! are driven through the real router, so what they assert is what a browser
//! would receive, and the fake service's `seen` log is what the *service*
//! received.
//!
//! The audio door is tested against a real directory tree — the same
//! `FILES_PATH` layout the deployment uses — because its whole job is refusing
//! paths that escape a school's directory. Every refusal test first proves the
//! target file exists, so a pass cannot come from the file simply being
//! missing.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use hezarfen_backend::ai::protocol::{
    Greeting, Hello, Request, Response, read_frame, write_frame,
};
use hezarfen_backend::ai::{AiBridge, BridgeConfig};
use hezarfen_backend::constant::{
    AI_ALPN, AI_PODCAST_CANCEL_CAPABILITY, AI_PODCAST_RESULT_CAPABILITY,
    AI_PODCAST_STATUS_CAPABILITY, AI_PODCAST_SUBMIT_CAPABILITY, AI_PROTOCOL,
};
use hezarfen_backend::module::{Module, ModuleSet};
use hezarfen_backend::tenant::{DEMO_SLUG, Slug};
use serde_json::{Value, json};

const TOKEN: &str = "shared-ai-token";

/// The four capabilities one podcast service declares.
const CAPABILITIES: [&str; 4] = [
    AI_PODCAST_SUBMIT_CAPABILITY,
    AI_PODCAST_STATUS_CAPABILITY,
    AI_PODCAST_RESULT_CAPABILITY,
    AI_PODCAST_CANCEL_CAPABILITY,
];

/// The job id the fake service mints, and the `audio_id` it answers — a path
/// relative to the school's output root, exactly as the real service writes it.
const JOB_ID: &str = "job-1";
const AUDIO_ID: &str = "ses/duz_okuma/bolum-1/episode.mp3";
/// A source id shaped like the backend ids the service is handed.
const SOURCE_ID: &str = "019732e3-7b00-7000-8000-00000000dead";

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

fn demo() -> Slug {
    Slug::try_new(DEMO_SLUG).expect("the demo slug")
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
/// registration) alive; dropping it is how a test simulates a crash.
struct FakeService {
    _endpoint: quinn::Endpoint,
    _control: (quinn::SendStream, quinn::RecvStream),
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
    /// Answer like the podcast service: a queued receipt, a status snapshot, a
    /// finished job's artifacts, a cancel verdict — each echoing the job id it
    /// was asked about, so a mis-routed id shows up in the answer.
    Podcast,
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
    let greeting: Greeting = read_frame(&mut recv).await.expect("read Greeting");
    match greeting {
        Greeting::Welcome { protocol, .. } => assert_eq!(protocol, AI_PROTOCOL),
        other => panic!("expected a welcome, got {other:?}"),
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    serve(conn.clone(), behaviour, Arc::clone(&seen));
    FakeService {
        _endpoint: endpoint,
        _control: (send, recv),
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
                    Behaviour::Podcast => Some(Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload: podcast_answer(&request),
                    }),
                    Behaviour::Fail { code, message } => Some(Response::Err {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        code,
                        message,
                    }),
                };
                if let Some(response) = response {
                    let _ = write_frame(&mut send, &response).await;
                    let _ = send.finish();
                    let _ = send.stopped().await;
                }
            });
        }
    });
}

/// What the podcast service would answer for this capability.
fn podcast_answer(request: &Request) -> Value {
    let job_id = request
        .payload
        .get("job_id")
        .cloned()
        .unwrap_or_else(|| json!(JOB_ID));
    match request.capability.as_str() {
        AI_PODCAST_SUBMIT_CAPABILITY => {
            json!({ "job_id": JOB_ID, "state": "queued", "eta_secs": 2700 })
        }
        AI_PODCAST_STATUS_CAPABILITY => json!({
            "job_id": job_id,
            "state": "running",
            "stage": "tts",
            "progress": 0.5,
            "error_code": null,
        }),
        AI_PODCAST_RESULT_CAPABILITY => json!({
            "job_id": job_id,
            "audio_id": AUDIO_ID,
            "duration_secs": 12.5,
            "script_id": "script-1",
            "audio_ids": [AUDIO_ID],
            "script_ids": ["script-1"],
            "format": "duz_okuma",
        }),
        AI_PODCAST_CANCEL_CAPABILITY => json!({ "job_id": job_id, "cancelled": true }),
        other => panic!("the fake podcast service was asked for `{other}`"),
    }
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

/// A registered podcast service offering all four capabilities, the router
/// wired to its bridge, and a logged-in student's cookie.
async fn podcast_app() -> (FakeService, Router, String) {
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("podcast", &CAPABILITIES),
        Behaviour::Podcast,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, _db) = common::app_with_ai(Some(bridge)).await;
    let cookie = common::login(&app, "ali").await;
    (service, app, cookie)
}

/// Submit one job and return the response.
async fn submit(app: &Router, cookie: &str, body: Value) -> common::Res {
    common::send(app, "POST", "/podcast/jobs", Some(cookie), Some(body)).await
}

// ------------------------------------------------------------ round trips --

#[tokio::test]
async fn a_submit_round_trips_the_source_and_format_to_the_service() {
    let (service, app, cookie) = podcast_app().await;

    let res = submit(
        &app,
        &cookie,
        json!({ "source_id": SOURCE_ID, "format": "duz_okuma" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(res.body["job_id"], JOB_ID);
    assert_eq!(res.body["state"], "queued");
    assert_eq!(res.body["eta_secs"], 2700);

    let seen = service.seen();
    assert_eq!(seen.len(), 1, "exactly one dispatch");
    assert_eq!(seen[0].capability, AI_PODCAST_SUBMIT_CAPABILITY);
    assert_eq!(seen[0].school, DEMO_SLUG, "the school rides the frame");
    assert_eq!(seen[0].payload["source_id"], SOURCE_ID);
    assert_eq!(seen[0].payload["format"], "duz_okuma");
}

/// The format is optional and the service applies its own default: an omitted
/// format must not appear on the wire at all, or the service would have to
/// distinguish "absent" from "empty".
#[tokio::test]
async fn an_omitted_format_is_not_sent_at_all() {
    let (service, app, cookie) = podcast_app().await;

    let res = submit(&app, &cookie, json!({ "source_id": SOURCE_ID })).await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);

    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].payload.get("format").is_none(),
        "no format was asked for, so none is sent: {}",
        seen[0].payload
    );
}

/// A blank source id is refused here, before the bridge: the service would
/// refuse it too, and a round trip to learn that is a round trip wasted.
#[tokio::test]
async fn a_blank_source_id_is_refused_before_dispatch() {
    let (service, app, cookie) = podcast_app().await;

    let res = submit(&app, &cookie, json!({ "source_id": "   " })).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(service.seen().is_empty(), "nothing reaches the service");
}

#[tokio::test]
async fn a_status_read_round_trips_the_job_id() {
    let (service, app, cookie) = podcast_app().await;

    let res = common::send(
        &app,
        "GET",
        &format!("/podcast/jobs/{JOB_ID}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], JOB_ID);
    assert_eq!(res.body["state"], "running");
    assert_eq!(res.body["stage"], "tts");
    assert_eq!(res.body["progress"], 0.5);
    assert!(res.body["error_code"].is_null());

    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].capability, AI_PODCAST_STATUS_CAPABILITY);
    assert_eq!(seen[0].payload["job_id"], JOB_ID);
}

#[tokio::test]
async fn a_result_read_round_trips_the_artifacts() {
    let (service, app, cookie) = podcast_app().await;

    let res = common::send(
        &app,
        "GET",
        &format!("/podcast/jobs/{JOB_ID}/result"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], JOB_ID);
    assert_eq!(res.body["audio_id"], AUDIO_ID);
    assert_eq!(res.body["duration_secs"], 12.5);
    assert_eq!(res.body["script_id"], "script-1");
    assert_eq!(res.body["audio_ids"], json!([AUDIO_ID]));
    assert_eq!(res.body["script_ids"], json!(["script-1"]));
    assert_eq!(res.body["format"], "duz_okuma");

    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].capability, AI_PODCAST_RESULT_CAPABILITY);
    assert_eq!(seen[0].payload["job_id"], JOB_ID);
}

#[tokio::test]
async fn a_cancel_round_trips_the_verdict() {
    let (service, app, cookie) = podcast_app().await;

    let res = common::send(
        &app,
        "POST",
        &format!("/podcast/jobs/{JOB_ID}/cancel"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["job_id"], JOB_ID);
    assert_eq!(res.body["cancelled"], true);

    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].capability, AI_PODCAST_CANCEL_CAPABILITY);
    assert_eq!(seen[0].payload["job_id"], JOB_ID);
}

// --------------------------------------------------------------- refusals --

/// With a bridge but no worker, every door answers the shared AI-unavailable
/// `503` — never a `500`, and never a dispatch into nothing. The audio door is
/// not in this list on purpose: it reads a local file and needs no service.
#[tokio::test]
async fn every_dispatching_door_is_503_with_no_worker_registered() {
    let bridge = bridge().await;
    let (app, _db) = common::app_with_ai(Some(bridge)).await;
    let cookie = common::login(&app, "ali").await;

    let doors = [
        ("POST", "/podcast/jobs".to_string(), true),
        ("GET", format!("/podcast/jobs/{JOB_ID}"), false),
        ("GET", format!("/podcast/jobs/{JOB_ID}/result"), false),
        ("POST", format!("/podcast/jobs/{JOB_ID}/cancel"), false),
    ];
    for (method, uri, body) in doors {
        let res = common::send(
            &app,
            method,
            &uri,
            Some(&cookie),
            body.then(|| json!({ "source_id": SOURCE_ID })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {uri}: {}",
            res.body
        );
        assert_eq!(res.body["error"], "no AI service is connected right now");
    }
}

/// The same doors with no bridge at all (the deployment never set
/// `AI_QUIC_ADDR`): still a `503`, with the other message.
#[tokio::test]
async fn every_dispatching_door_is_503_when_the_bridge_is_off() {
    let (app, _db) = common::app_with_ai(None).await;
    let cookie = common::login(&app, "ali").await;

    let res = submit(&app, &cookie, json!({ "source_id": SOURCE_ID })).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
    assert_eq!(
        res.body["error"],
        "the AI service is not enabled on this deployment"
    );
}

/// The service's own refusal code decides the HTTP status, so a client can
/// branch the same way it does on the bridge: `busy` is the service at
/// capacity (transient, `503`), `not_found` is a job it never had (`404`), and
/// `not_ready` is a job that has not finished (`409`).
#[tokio::test]
async fn a_service_refusal_keeps_its_code_and_status() {
    let cases = [
        ("busy", StatusCode::SERVICE_UNAVAILABLE),
        ("not_found", StatusCode::NOT_FOUND),
        ("not_ready", StatusCode::CONFLICT),
    ];
    for (code, expected) in cases {
        let bridge = bridge().await;
        let _service = connect_service(
            &bridge,
            hello("podcast", &CAPABILITIES),
            Behaviour::Fail {
                code: code.to_string(),
                message: format!("the service refused with {code}"),
            },
        )
        .await;
        await_workers(&bridge, 1).await;
        let (app, _db) = common::app_with_ai(Some(bridge)).await;
        let cookie = common::login(&app, "ali").await;

        let res = common::send(
            &app,
            "GET",
            &format!("/podcast/jobs/{JOB_ID}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, expected, "{code}: {}", res.body);
        assert_eq!(res.body["error"], code);
    }
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

    let res = common::send(
        &app,
        "GET",
        &format!("/podcast/jobs/{JOB_ID}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(res.body["module"], "chatbot");
}

/// Authentication is the whole gate for the dispatching doors: no cookie is a
/// `401`, not a dispatch under somebody else's name.
#[tokio::test]
async fn the_dispatching_doors_require_a_session() {
    let (service, app, _cookie) = podcast_app().await;

    let res = common::send(
        &app,
        "POST",
        "/podcast/jobs",
        None,
        Some(json!({ "source_id": SOURCE_ID })),
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
    assert!(service.seen().is_empty(), "an anonymous call dispatches nothing");
}

// -------------------------------------------------------------- audio door --

/// Write `bytes` at `relative` under the demo school's blob directory and
/// return the absolute path it landed at.
fn school_file(relative: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = common::blob_dir().join(relative);
    std::fs::create_dir_all(path.parent().expect("parent dir")).expect("create the directory tree");
    std::fs::write(&path, bytes).expect("write the file");
    path
}

/// One raw `GET /podcast/audio?path=…`.
async fn fetch_audio(
    app: &Router,
    cookie: Option<&str>,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let uri = format!("/podcast/audio?path={path}");
    common::send_raw(app, "GET", &uri, cookie, None, Vec::new()).await
}

/// The happy path: a file under the school's own directory streams back with
/// its bytes and an audio content type.
#[tokio::test]
async fn the_audio_door_serves_a_file_under_the_schools_directory() {
    let (app, _db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    let bytes = b"ID3\x03\x00\x00\x00fake-mp3-bytes".to_vec();
    school_file("podcast/serve/ok.mp3", &bytes);

    let (status, headers, body) = fetch_audio(&app, Some(&cookie), "podcast/serve/ok.mp3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "audio/mpeg");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");
    assert_eq!(body, bytes, "the bytes are the file's, unchanged");
}

/// Without a session the door is a `401` — and the very same path serves `200`
/// with one, so the refusal is authorization and not a missing file.
#[tokio::test]
async fn the_audio_door_refuses_an_unauthenticated_caller() {
    let (app, _db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    let bytes = b"authorized-only".to_vec();
    school_file("podcast/auth/only.mp3", &bytes);

    let (anonymous, _, _) = fetch_audio(&app, None, "podcast/auth/only.mp3").await;
    assert_eq!(anonymous, StatusCode::UNAUTHORIZED);

    let (authorized, _, body) = fetch_audio(&app, Some(&cookie), "podcast/auth/only.mp3").await;
    assert_eq!(authorized, StatusCode::OK);
    assert_eq!(body, bytes);
}

/// A `..` segment is refused even though the file it names exists and the file
/// *inside* the school's directory is served — the refusal is the shape check,
/// not a missing target.
#[tokio::test]
async fn the_audio_door_refuses_a_dotdot_segment_to_an_existing_file() {
    let (app, _db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    let inside = b"inside the school".to_vec();
    let outside = b"outside the school".to_vec();
    school_file("podcast/traversal/inside.mp3", &inside);
    // What `../` would reach: the deployment-wide files root, one level above
    // the school's own directory.
    let escaped = common::files_dir().join("traversal-outside.mp3");
    std::fs::write(&escaped, &outside).expect("write the outside file");
    assert!(escaped.is_file(), "the traversal target really exists");

    let (served, _, body) = fetch_audio(&app, Some(&cookie), "podcast/traversal/inside.mp3").await;
    assert_eq!(served, StatusCode::OK);
    assert_eq!(body, inside);

    let (refused, _, body) =
        fetch_audio(&app, Some(&cookie), "podcast/traversal/../../traversal-outside.mp3").await;
    assert_eq!(refused, StatusCode::BAD_REQUEST, "{:?}", String::from_utf8_lossy(&body));
    assert_ne!(body, outside, "the escaped file's bytes never came back");
}

/// An absolute path is refused even though the file it names exists — and a
/// naive `root.join(path)` would have *served* it, since joining an absolute
/// path replaces the base.
#[tokio::test]
async fn the_audio_door_refuses_an_absolute_path_to_an_existing_file() {
    let (app, _db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    let outside = common::files_dir().join("absolute-outside.mp3");
    std::fs::write(&outside, b"not for the audio door").expect("write the outside file");

    let (status, _, body) = fetch_audio(
        &app,
        Some(&cookie),
        outside.to_str().expect("a UTF-8 temp path"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{:?}", String::from_utf8_lossy(&body));
    assert_ne!(body, b"not for the audio door");
}

/// A symlink inside the school's directory is refused when it resolves out of
/// it — the escape a plain "does the path start with the root" prefix check
/// would miss, since the link itself sits inside the root.
#[tokio::test]
async fn the_audio_door_refuses_a_symlink_that_escapes_the_school() {
    let (app, _db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    let target = common::files_dir().join("symlink-outside.mp3");
    std::fs::write(&target, b"reached through a link").expect("write the target");
    let link = common::blob_dir().join("podcast/symlink/escape.mp3");
    std::fs::create_dir_all(link.parent().expect("parent dir")).expect("create the directory tree");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&target, &link).expect("create the symlink");
    assert!(target.is_file(), "the link's target really exists");

    let (status, _, body) = fetch_audio(&app, Some(&cookie), "podcast/symlink/escape.mp3").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{:?}", String::from_utf8_lossy(&body));
    assert_ne!(body, b"reached through a link");
}

/// Another school's episode is invisible even when the caller knows its exact
/// path: the demo caller's request resolves under the *demo* directory, where
/// that path does not exist — though the file itself does, one school over.
#[tokio::test]
async fn the_audio_door_never_reaches_another_schools_file() {
    let (app, _db) = common::app_and_db().await;
    let cookie = common::login(&app, "ali").await;
    let other = common::files_dir().join("baska-okul/podcast/ses/episode.mp3");
    std::fs::create_dir_all(other.parent().expect("parent dir")).expect("create the directory tree");
    std::fs::write(&other, b"the other school's episode").expect("write the file");
    assert!(other.is_file());

    let (status, _, _) = fetch_audio(&app, Some(&cookie), "podcast/ses/episode.mp3").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The door needs no AI bridge at all: a produced file is served with the
/// service long gone.
#[tokio::test]
async fn the_audio_door_needs_no_service() {
    let (app, _db) = common::app_with_ai(None).await;
    let cookie = common::login(&app, "ali").await;
    let bytes = b"produced before the service left".to_vec();
    school_file("podcast/offline/episode.mp3", &bytes);

    let (status, _, body) = fetch_audio(&app, Some(&cookie), "podcast/offline/episode.mp3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
}
