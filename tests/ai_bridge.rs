//! End-to-end tests for the AI bridge, against a fake AI service.
//!
//! Everything here runs over a real QUIC socket on loopback: a real
//! handshake, real TLS, real streams. The only thing pretended is the model —
//! [`Behaviour`] stands in for what a service would do with a request. That
//! keeps the transport honest: a bug in framing, stream lifetime, registration
//! or timeout shows up here rather than the first time a Python service dials
//! in.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use hezarfen_backend::ai::protocol::{
    Greeting, Hello, RejectCode, Request, Response, read_frame, write_frame,
};
use hezarfen_backend::ai::{AiBridge, AiError, BridgeConfig};
use hezarfen_backend::constant::{AI_ALPN, AI_MAX_CONCURRENT_PER_WORKER, AI_PROTOCOL};
use hezarfen_backend::module::ModuleSet;
use hezarfen_backend::tenant::{DEMO_SLUG, Slug};
use serde_json::{Value, json};

const TOKEN: &str = "shared-ai-token";

// ---------------------------------------------------------------- harness --

/// Start a bridge on an ephemeral port with the given per-request deadline.
async fn bridge_with_timeout(timeout: Duration) -> AiBridge {
    AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: timeout,
    })
    .await
    .expect("bridge binds on an ephemeral port")
}

async fn bridge() -> AiBridge {
    bridge_with_timeout(Duration::from_secs(5)).await
}

/// A QUIC client endpoint that trusts exactly the bridge's own certificate —
/// the same pinning an AI service does in production, rather than a
/// verification bypass that would let a broken certificate pass unnoticed.
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
    // Requests arrive as *server*-initiated streams, so this is the ceiling on
    // how many the bridge may have open toward this service at once.
    transport.max_concurrent_bidi_streams(256u32.into());
    config.transport_config(Arc::new(transport));

    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
    endpoint.set_default_client_config(config);
    endpoint
}

/// The demo school, which every dispatch in this suite is made on behalf of.
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

/// A connected fake service. Holding it keeps the connection (and therefore
/// the registration) alive; dropping it is how a test simulates a crash.
struct FakeService {
    _endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    /// The control stream must outlive the handshake — the bridge treats its
    /// closure as a goodbye.
    _control: (quinn::SendStream, quinn::RecvStream),
    /// Every request this service received, in arrival order.
    seen: Arc<Mutex<Vec<Request>>>,
}

impl FakeService {
    fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

/// Perform the handshake only, returning whatever the bridge answered. Used by
/// the rejection tests, which never get as far as serving requests.
async fn shake_hands(
    bridge: &AiBridge,
    hello: Hello,
) -> (
    quinn::Endpoint,
    quinn::Connection,
    quinn::SendStream,
    quinn::RecvStream,
    Greeting,
) {
    let endpoint = client_endpoint(bridge);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .expect("dial")
        .await
        .expect("QUIC handshake");
    let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
    write_frame(&mut send, &hello).await.expect("send Hello");
    let greeting: Greeting = read_frame(&mut recv).await.expect("read Greeting");
    (endpoint, conn, send, recv, greeting)
}

/// What the fake service does with each request it receives.
#[derive(Clone)]
enum Behaviour {
    /// Answer `{"echo": <payload>, "capability": <capability>}`.
    Echo,
    /// Answer after a delay — a slow model.
    SlowEcho(Duration),
    /// Answer with a fixed payload, whatever the request.
    Answer(Value),
    /// Answer a chatbot turn with a fixed reply text (`{"text": ...}`).
    Reply(String),
    /// Answer a chatbot turn with `cevap::<the prompt it was given>`. The reply
    /// carries the question inside it, so an answer stored against the wrong
    /// prompt is visible in the row rather than merely suspected.
    EchoPrompt,
    /// Answer with a handled failure.
    Fail { code: String, message: String },
    /// Accept the stream and never answer, holding it open.
    Silent,
    /// Answer with a trace id belonging to no request.
    WrongId,
    /// Write bytes that are not a valid frame body.
    Garbage,
    /// Wait until `n` requests are in flight before answering any of them.
    /// Only completes if the bridge really has `n` streams open at once.
    Rendezvous(Arc<tokio::sync::Barrier>),
}

/// Dial, register, and start serving requests with `behaviour`.
async fn connect_service(bridge: &AiBridge, hello: Hello, behaviour: Behaviour) -> FakeService {
    let (endpoint, conn, send, recv, greeting) = shake_hands(bridge, hello).await;
    match greeting {
        Greeting::Welcome { protocol, .. } => assert_eq!(protocol, AI_PROTOCOL),
        other => panic!("expected a welcome, got {other:?}"),
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    serve(conn.clone(), behaviour, Arc::clone(&seen));
    FakeService {
        _endpoint: endpoint,
        conn,
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
                    Behaviour::Echo => Some(echo(&request)),
                    Behaviour::SlowEcho(d) => {
                        tokio::time::sleep(d).await;
                        Some(echo(&request))
                    }
                    Behaviour::Rendezvous(barrier) => {
                        barrier.wait().await;
                        Some(echo(&request))
                    }
                    Behaviour::Answer(payload) => Some(Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload,
                    }),
                    Behaviour::Reply(text) => Some(Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload: json!({ "text": text }),
                    }),
                    Behaviour::EchoPrompt => {
                        let asked = request.payload["message"]
                            .as_str()
                            .unwrap_or("<no message>");
                        Some(Response::Ok {
                            id: request.id.clone(),
                            school: request.school.clone(),
                            payload: json!({ "text": format!("cevap::{asked}") }),
                        })
                    }
                    Behaviour::Fail { code, message } => Some(Response::Err {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        code,
                        message,
                    }),
                    Behaviour::WrongId => Some(Response::Ok {
                        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
                        school: request.school.clone(),
                        payload: json!("from some other request"),
                    }),
                    Behaviour::Garbage => {
                        let body = b"} not json {";
                        let _ = send.write_all(&(body.len() as u32).to_be_bytes()).await;
                        let _ = send.write_all(body).await;
                        let _ = send.finish();
                        None
                    }
                    Behaviour::Silent => {
                        // Hold the stream open forever. Dropping `send` here
                        // would reset the stream and surface as a transport
                        // error instead of the timeout under test.
                        std::future::pending::<()>().await;
                        None
                    }
                };
                if let Some(response) = response {
                    let _ = write_frame(&mut send, &response).await;
                    let _ = send.finish();
                    // Let the peer read before the stream drops.
                    let _ = send.stopped().await;
                }
            });
        }
    });
}

fn echo(request: &Request) -> Response {
    Response::Ok {
        id: request.id.clone(),
        school: request.school.clone(),
        payload: json!({ "echo": request.payload, "capability": request.capability }),
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

/// Wait until the bridge shows `expected` requests in flight on any worker.
async fn await_inflight(bridge: &AiBridge, expected: usize) {
    for _ in 0..300 {
        if bridge.workers().iter().map(|w| w.inflight).sum::<usize>() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("in-flight never reached {expected}: {:?}", bridge.workers());
}

// ------------------------------------------------------------ happy paths --

#[tokio::test]
async fn a_service_registers_and_a_request_round_trips() {
    let bridge = bridge().await;
    let service = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;

    let workers = bridge.workers();
    assert_eq!(workers[0].service, "ocr");
    assert_eq!(workers[0].capabilities, vec!["ocr.extract".to_string()]);
    assert!(bridge.has_capability("ocr.extract"));

    let answer = bridge
        .dispatch(&demo(), "ocr.extract", json!({ "image": "abc" }))
        .await
        .expect("the service answered");
    assert_eq!(answer["echo"]["image"], "abc");
    assert_eq!(answer["capability"], "ocr.extract");

    // The request carried the deadline the bridge intends to honour, so the
    // service can give up on its own rather than answering into a closed
    // stream.
    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].deadline_ms, 5_000);
    assert!(!seen[0].id.is_empty());

    // The slot is handed back once the answer lands.
    assert_eq!(bridge.workers()[0].inflight, 0);
}

#[tokio::test]
async fn concurrent_requests_share_one_connection_without_blocking_each_other() {
    // The whole reason for QUIC: 12 requests, one connection, no correlation
    // ids. The service refuses to answer any of them until all 12 have
    // arrived, so this only completes if the bridge really had 12 streams open
    // at once. Head-of-line blocking or accidental serialization deadlocks it.
    const N: usize = 12;
    let bridge = bridge_with_timeout(Duration::from_secs(20)).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut hello = hello("ocr", &["ocr.extract"]);
    hello.max_concurrent = Some(N as u32);
    let service =
        connect_service(&bridge, hello, Behaviour::Rendezvous(Arc::clone(&barrier))).await;
    await_workers(&bridge, 1).await;

    let calls = (0..N).map(|i| {
        let bridge = bridge.clone();
        tokio::spawn(async move {
            bridge
                .dispatch(&demo(), "ocr.extract", json!({ "n": i }))
                .await
        })
    });
    let answers = futures_util::future::join_all(calls).await;

    let mut got: Vec<u64> = answers
        .into_iter()
        .map(|joined| joined.expect("task did not panic").expect("dispatch ok"))
        .map(|value| value["echo"]["n"].as_u64().expect("echoed n"))
        .collect();
    got.sort_unstable();
    assert_eq!(got, (0..N as u64).collect::<Vec<_>>());

    // Every answer went back to its own caller: no payload crossed streams.
    assert_eq!(service.seen().len(), N);
    assert_eq!(bridge.workers()[0].inflight, 0);
}

#[tokio::test]
async fn requests_route_to_the_service_declaring_the_capability() {
    let bridge = bridge().await;
    let ocr = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    let grader = connect_service(&bridge, hello("grader", &["grade.essay"]), Behaviour::Echo).await;
    await_workers(&bridge, 2).await;

    bridge
        .dispatch(&demo(), "grade.essay", json!("text"))
        .await
        .unwrap();
    bridge
        .dispatch(&demo(), "ocr.extract", json!("png"))
        .await
        .unwrap();

    assert_eq!(ocr.seen().len(), 1, "ocr only saw its own capability");
    assert_eq!(ocr.seen()[0].capability, "ocr.extract");
    assert_eq!(grader.seen().len(), 1);
    assert_eq!(grader.seen()[0].capability, "grade.essay");
}

#[tokio::test]
async fn a_second_connection_from_the_same_service_is_a_second_worker() {
    // How an AI service scales: another process dials in, and load spreads
    // across both without any backend config change.
    let bridge = bridge().await;
    let _a = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    let _b = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 2).await;

    let ids: std::collections::HashSet<_> = bridge.workers().into_iter().map(|w| w.id).collect();
    assert_eq!(ids.len(), 2, "each connection gets its own worker id");
}

#[tokio::test]
async fn a_declared_concurrency_is_clamped_and_reported() {
    let bridge = bridge().await;
    let mut greedy = hello("ocr", &["ocr.extract"]);
    greedy.max_concurrent = Some(100_000);
    let _service = connect_service(&bridge, greedy, Behaviour::Echo).await;
    await_workers(&bridge, 1).await;
    assert_eq!(
        bridge.workers()[0].max_concurrent,
        AI_MAX_CONCURRENT_PER_WORKER,
        "a service cannot talk itself into unbounded fan-in"
    );
}

// --------------------------------------------------------- handshake gate --

#[tokio::test]
async fn a_bad_token_is_rejected_and_never_registers() {
    let bridge = bridge().await;
    let mut wrong = hello("ocr", &["ocr.extract"]);
    wrong.token = "not-the-token".into();
    let (_endpoint, _conn, _send, _recv, greeting) = shake_hands(&bridge, wrong).await;

    assert!(
        matches!(
            greeting,
            Greeting::Rejected {
                code: RejectCode::Unauthorized,
                ..
            }
        ),
        "got {greeting:?}"
    );
    assert!(bridge.workers().is_empty());
    assert!(matches!(
        bridge.dispatch(&demo(), "ocr.extract", json!(null)).await,
        Err(AiError::NoWorker(_))
    ));
}

#[tokio::test]
async fn a_foreign_protocol_version_is_rejected_with_both_versions_named() {
    let bridge = bridge().await;
    let mut future = hello("ocr", &["ocr.extract"]);
    future.protocol = "hab/99".into();
    let (_e, _c, _s, _r, greeting) = shake_hands(&bridge, future).await;

    match greeting {
        Greeting::Rejected { code, message } => {
            assert_eq!(code, RejectCode::UnsupportedProtocol);
            // The service operator has to know which side to change.
            assert!(message.contains(AI_PROTOCOL), "{message}");
            assert!(message.contains("hab/99"), "{message}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert!(bridge.workers().is_empty());
}

#[tokio::test]
async fn a_service_declaring_no_capabilities_is_rejected() {
    // It could never be routed anything, so accepting it would only make the
    // registry lie about what is available.
    let bridge = bridge().await;
    let (_e, _c, _s, _r, greeting) = shake_hands(&bridge, hello("idle", &[])).await;
    assert!(
        matches!(
            greeting,
            Greeting::Rejected {
                code: RejectCode::NoCapabilities,
                ..
            }
        ),
        "got {greeting:?}"
    );
    assert!(bridge.workers().is_empty());
}

#[tokio::test]
async fn an_unreadable_hello_is_rejected_as_malformed() {
    let bridge = bridge().await;
    let endpoint = client_endpoint(&bridge);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let body = b"hello?";
    send.write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    send.write_all(body).await.unwrap();

    let greeting: Greeting = read_frame(&mut recv).await.expect("a rejection came back");
    assert!(
        matches!(
            greeting,
            Greeting::Rejected {
                code: RejectCode::Malformed,
                ..
            }
        ),
        "got {greeting:?}"
    );
    assert!(bridge.workers().is_empty());
}

#[tokio::test]
async fn a_connection_that_never_says_hello_is_dropped_not_kept() {
    // An unauthenticated peer must not be able to hold a connection open by
    // simply staying quiet. AI_HANDSHAKE_TIMEOUT_SECS is 10s, so this asserts
    // the pre-handshake state rather than waiting it out: nothing registers,
    // and the bridge stays usable for everyone else.
    let bridge = bridge().await;
    let endpoint = client_endpoint(&bridge);
    let _silent = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .unwrap()
        .await
        .unwrap();

    let _real = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;
    assert_eq!(bridge.workers()[0].service, "ocr");
}

// ------------------------------------------------------------- sad paths --

#[tokio::test]
async fn an_unknown_capability_fails_fast_instead_of_waiting() {
    let bridge = bridge().await;
    let _service = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;

    let err = bridge
        .dispatch(&demo(), "grade.essay", json!(null))
        .await
        .expect_err("nothing serves that capability");
    assert!(
        matches!(&err, AiError::NoWorker(c) if c == "grade.essay"),
        "{err}"
    );
    assert!(!bridge.has_capability("grade.essay"));
}

#[tokio::test]
async fn a_service_error_frame_surfaces_as_a_remote_error() {
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("ocr", &["ocr.extract"]),
        Behaviour::Fail {
            code: "unsupported_image".into(),
            message: "only png and jpeg".into(),
        },
    )
    .await;
    await_workers(&bridge, 1).await;

    let err = bridge
        .dispatch(&demo(), "ocr.extract", json!({ "image": "x" }))
        .await
        .unwrap_err();
    match &err {
        AiError::Remote { code, message } => {
            assert_eq!(code, "unsupported_image");
            assert_eq!(message, "only png and jpeg");
        }
        other => panic!("expected a remote error, got {other}"),
    }
    // The service considered the request and said no — retrying changes
    // nothing, and the slot is already back.
    assert!(!err.is_retryable());
    assert_eq!(bridge.workers()[0].inflight, 0);
}

#[tokio::test]
async fn a_silent_service_times_out_and_gives_the_slot_back() {
    let bridge = bridge_with_timeout(Duration::from_millis(300)).await;
    let service = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Silent).await;
    await_workers(&bridge, 1).await;

    let err = bridge
        .dispatch(&demo(), "ocr.extract", json!(null))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Timeout(300)), "{err}");
    // The request did reach the service — this is a timeout, not a delivery
    // failure, which is exactly why the error cannot promise it did not run.
    assert_eq!(service.seen().len(), 1);
    // An abandoned request must not leak the worker's capacity.
    assert_eq!(bridge.workers()[0].inflight, 0);
    // And the worker is immediately usable again.
    assert!(
        bridge
            .dispatch(&demo(), "ocr.extract", json!(null))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_per_call_timeout_overrides_the_default() {
    let bridge = bridge_with_timeout(Duration::from_millis(100)).await;
    let _service = connect_service(
        &bridge,
        hello("ocr", &["ocr.extract"]),
        Behaviour::SlowEcho(Duration::from_millis(400)),
    )
    .await;
    await_workers(&bridge, 1).await;

    // The default deadline is too short for this service...
    assert!(matches!(
        bridge.dispatch(&demo(), "ocr.extract", json!(null)).await,
        Err(AiError::Timeout(100))
    ));
    // ...but a capability known to be slow can ask for more.
    let answer = bridge
        .dispatch_with_timeout(
            &demo(),
            "ocr.extract",
            json!("slow"),
            Duration::from_secs(5),
        )
        .await
        .expect("the longer deadline held");
    assert_eq!(answer["echo"], "slow");
}

#[tokio::test]
async fn a_full_worker_reports_busy_rather_than_queueing() {
    let bridge = bridge_with_timeout(Duration::from_secs(10)).await;
    let mut narrow = hello("ocr", &["ocr.extract"]);
    narrow.max_concurrent = Some(1);
    let _service = connect_service(&bridge, narrow, Behaviour::Silent).await;
    await_workers(&bridge, 1).await;

    let hog = {
        let bridge = bridge.clone();
        tokio::spawn(async move { bridge.dispatch(&demo(), "ocr.extract", json!(null)).await })
    };
    await_inflight(&bridge, 1).await;

    let err = bridge
        .dispatch(&demo(), "ocr.extract", json!(null))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, AiError::Busy(c) if c == "ocr.extract"),
        "{err}"
    );
    // Busy is the caller's cue to come back — the capacity exists, unlike
    // NoWorker.
    assert!(err.is_retryable());
    hog.abort();
}

#[tokio::test]
async fn an_answer_carrying_the_wrong_trace_id_is_refused() {
    // Correlation comes from the stream, so a mismatched id cannot misroute an
    // answer — but it does mean the service lost track of whose work this is,
    // and that payload must not be returned as this request's result.
    let bridge = bridge().await;
    let _service =
        connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::WrongId).await;
    await_workers(&bridge, 1).await;

    let err = bridge
        .dispatch(&demo(), "ocr.extract", json!(null))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::IdMismatch { .. }), "{err}");
    assert_eq!(bridge.workers()[0].inflight, 0);
}

#[tokio::test]
async fn a_malformed_answer_is_a_protocol_error_not_a_hang() {
    let bridge = bridge().await;
    let _service =
        connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Garbage).await;
    await_workers(&bridge, 1).await;

    let err = bridge
        .dispatch(&demo(), "ocr.extract", json!(null))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Protocol(_)), "{err}");
    assert!(!err.is_retryable(), "a broken peer will keep being broken");
}

// ------------------------------------------------------------- lifecycle --

#[tokio::test]
async fn a_disconnected_service_is_deregistered() {
    let bridge = bridge().await;
    let service = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;
    bridge
        .dispatch(&demo(), "ocr.extract", json!(null))
        .await
        .unwrap();

    service.conn.close(0u32.into(), b"service shutting down");
    drop(service);
    await_workers(&bridge, 0).await;

    assert!(!bridge.has_capability("ocr.extract"));
    let err = bridge
        .dispatch(&demo(), "ocr.extract", json!(null))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::NoWorker(_)), "{err}");
}

#[tokio::test]
async fn a_service_that_reconnects_serves_again() {
    // Services restart — a deploy, a crash, an OOM. The backend must recover
    // without its own restart.
    let bridge = bridge().await;
    let first = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;
    first.conn.close(0u32.into(), b"restarting");
    drop(first);
    await_workers(&bridge, 0).await;

    let second = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;
    let answer = bridge
        .dispatch(&demo(), "ocr.extract", json!("again"))
        .await
        .unwrap();
    assert_eq!(answer["echo"], "again");
    assert_eq!(second.seen().len(), 1);
}

#[tokio::test]
async fn losing_one_service_leaves_the_other_serving() {
    let bridge = bridge().await;
    let doomed = connect_service(&bridge, hello("ocr-a", &["ocr.extract"]), Behaviour::Echo).await;
    let survivor =
        connect_service(&bridge, hello("ocr-b", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 2).await;

    doomed.conn.close(0u32.into(), b"bye");
    drop(doomed);
    await_workers(&bridge, 1).await;

    for _ in 0..4 {
        bridge
            .dispatch(&demo(), "ocr.extract", json!("x"))
            .await
            .unwrap();
    }
    assert_eq!(
        survivor.seen().len(),
        4,
        "every request landed on the surviving worker"
    );
}

#[tokio::test]
async fn the_certificate_fingerprint_matches_what_a_service_pins() {
    // The startup log prints this value and services pin it; if it did not
    // describe the certificate actually presented, pinning would fail in
    // production and nowhere else.
    let bridge = bridge().await;
    use sha2::{Digest, Sha256};
    let expected = hex::encode(Sha256::digest(bridge.certificate().as_ref()));
    assert_eq!(bridge.certificate_fingerprint(), expected);

    // And the pinned certificate is genuinely the one that authenticates the
    // handshake: client_endpoint trusts nothing else.
    let _service = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;
}

#[tokio::test]
async fn a_service_pinning_the_wrong_certificate_cannot_connect() {
    // Two bridges, two self-signed certificates. Trusting the wrong one must
    // fail the TLS handshake — otherwise the pin is decorative.
    let real = bridge().await;
    let other = bridge().await;
    hezarfen_backend::ai::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(other.certificate()).unwrap();
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    ));
    config.transport_config(Arc::new(quinn::TransportConfig::default()));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);

    let result = endpoint
        .connect(real.local_addr().unwrap(), "localhost")
        .unwrap()
        .await;
    assert!(result.is_err(), "the wrong pin must not connect");
    assert!(real.workers().is_empty());
}

#[tokio::test]
async fn the_bridge_refuses_to_start_without_a_token() {
    // Listening unauthenticated would expose every AI capability to anything
    // that can reach the port.
    let err = AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: "   ".to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: Duration::from_secs(5),
    })
    .await
    .err()
    .expect("a blank token is not a token");
    assert!(matches!(err, AiError::Setup(m) if m.contains("AI_SHARED_TOKEN")));
}

#[tokio::test]
async fn closing_the_bridge_stops_accepting_services() {
    let bridge = bridge().await;
    let addr = bridge.local_addr().unwrap();
    bridge.close();

    let endpoint = client_endpoint(&bridge);
    let result = endpoint.connect(addr, "localhost").unwrap().await;
    assert!(result.is_err(), "a closed bridge accepts nothing");
}

/// A payload that would not survive a naive `read_to_end`-style codec: several
/// hundred kilobytes, forcing the frame across many QUIC packets.
#[tokio::test]
async fn a_large_payload_survives_the_round_trip_intact() {
    let bridge = bridge_with_timeout(Duration::from_secs(20)).await;
    let _service = connect_service(&bridge, hello("ocr", &["ocr.extract"]), Behaviour::Echo).await;
    await_workers(&bridge, 1).await;

    let blob: String = std::iter::repeat_n('x', 512 * 1024).collect();
    let answer = bridge
        .dispatch(&demo(), "ocr.extract", json!({ "image": blob }))
        .await
        .expect("a multi-packet frame round-trips");
    let echoed = answer["echo"]["image"].as_str().expect("string came back");
    assert_eq!(echoed.len(), 512 * 1024);
    assert!(echoed.bytes().all(|b| b == b'x'));
}

/// Sanity: the value used as the wire protocol id and the ALPN agree, since a
/// mismatch would let an incompatible service past the TLS gate only to be
/// refused a frame later.
#[tokio::test]
async fn the_alpn_and_the_protocol_id_are_the_same_string() {
    assert_eq!(AI_ALPN, AI_PROTOCOL.as_bytes());
}

// ------------------------------------------------- certificate discovery --

/// Drive `GET /ai/certificate` against a router wired to `ai`, returning
/// (status, body).
async fn fetch_certificate(ai: Option<AiBridge>) -> (axum::http::StatusCode, Value) {
    use tower::ServiceExt;
    let (tenants, _db) = common::mem_deployment().await;
    let app = hezarfen_backend::build_router(hezarfen_backend::state::AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: std::env::temp_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/ai/certificate")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .expect("router answered");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

#[tokio::test]
async fn the_certificate_endpoint_publishes_a_usable_trust_anchor() {
    // The whole point of the endpoint: a service fetches this PEM, trusts it,
    // and the QUIC handshake succeeds. Anything less (well-formed PEM that is
    // not the listener's certificate) would pass a shape assertion and fail in
    // production, so this test connects for real.
    let bridge = bridge().await;
    let (status, body) = fetch_certificate(Some(bridge.clone())).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["protocol"], AI_PROTOCOL);
    assert_eq!(body["fingerprint_sha256"], bridge.certificate_fingerprint());

    let pem = body["certificate_pem"].as_str().expect("pem string");
    let chain = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .expect("the published PEM parses");
    assert_eq!(chain.len(), 1);

    hezarfen_backend::ai::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(chain[0].clone())
        .expect("trust the published leaf");
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(64u32.into());
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);

    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .unwrap()
        .await
        .expect("a service that pinned the published PEM connects");

    // And it can go on to register and serve, so the published certificate is
    // sufficient on its own.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, &hello("ocr", &["ocr.extract"]))
        .await
        .unwrap();
    let greeting: Greeting = read_frame(&mut recv).await.unwrap();
    assert!(matches!(greeting, Greeting::Welcome { .. }), "{greeting:?}");
}

#[tokio::test]
async fn the_certificate_endpoint_is_404_when_the_bridge_is_off() {
    // A deployment that never enabled the bridge has no certificate to
    // describe; a service pointed at it should fail loudly rather than read a
    // null and dial into nothing.
    let (status, body) = fetch_certificate(None).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not found");
}

#[tokio::test]
async fn a_regenerated_certificate_is_republished() {
    // Restarting the backend re-selfsigns, which is exactly why a service must
    // re-fetch on reconnect instead of pinning once at startup. Two bridges
    // stand in for before/after a restart.
    let before = bridge().await;
    let after = bridge().await;
    let (_, first) = fetch_certificate(Some(before.clone())).await;
    let (_, second) = fetch_certificate(Some(after.clone())).await;

    assert_ne!(
        first["fingerprint_sha256"], second["fingerprint_sha256"],
        "a fresh boot means a fresh certificate"
    );
    assert_eq!(
        second["fingerprint_sha256"],
        after.certificate_fingerprint()
    );
}

/// Helper sanity: `Value` is what dispatch returns, so nothing above depends
/// on a typed payload leaking into the transport.
#[allow(dead_code)]
fn payloads_are_opaque(v: Value) -> Value {
    v
}

// ------------------------------------------------------------- chat relay --
//
// The chatbot end to end: a real HTTP handler, a real QUIC round trip, and a
// fake service standing in for the model. These pin the `chat.reply` payload
// contract another language implements against, and the rule that a turn
// always settles — whatever the service answers.

use axum::Router;
use axum::http::StatusCode;
use hezarfen_backend::constant::{AI_CHAT_CAPABILITY, DEFAULT_MAX_CHATBOT_MESSAGE_LEN};
use hezarfen_backend::database::Database;
use uuid::Uuid;

/// A router wired to `bridge`, plus a handle to its in-memory database.
async fn chat_app(bridge: &AiBridge) -> (Router, Database) {
    let (tenants, db) = common::mem_deployment().await;
    let app = hezarfen_backend::build_router(hezarfen_backend::state::AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai: Some(bridge.clone()),
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    (app, db)
}

/// Register, log in, and open one thread. Returns (session cookie, thread id).
async fn chat_user(app: &Router, name: &str) -> (String, String) {
    let cookie = common::login(app, name).await;
    let res = common::send(
        app,
        "POST",
        "/chatbot/threads",
        Some(&cookie),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let id = common::id_of(&res.body);
    (cookie, id)
}

/// Ask one question (asserts `202`) and return the reserved assistant row's id.
async fn ask(app: &Router, cookie: &str, thread: &str, text: &str) -> String {
    let res = common::send(
        app,
        "POST",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(cookie),
        Some(json!({ "content": text })),
    )
    .await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    res.body["message_id"]
        .as_str()
        .expect("message_id")
        .to_string()
}

/// Poll a turn until it leaves `pending`. Bounded polling rather than a sleep
/// sized to the answering task: the round trip settles when it settles.
async fn settled(app: &Router, cookie: &str, thread: &str, mid: &str) -> Value {
    for _ in 0..500 {
        let res = common::send(
            app,
            "GET",
            &format!("/chatbot/threads/{thread}/messages/{mid}"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        if res.body["status"] != "pending" {
            return res.body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the turn never settled");
}

#[tokio::test]
async fn a_chat_turn_settles_complete_with_the_services_text() {
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Reply("F = ma".into()),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, _db) = chat_app(&bridge).await;
    let (cookie, thread) = chat_user(&app, "ali").await;

    let mid = ask(&app, &cookie, &thread, "ikinci yasa nedir?").await;
    let turn = settled(&app, &cookie, &thread, &mid).await;
    assert_eq!(turn["status"], "complete", "{turn}");
    assert_eq!(turn["content"], "F = ma");
    assert_eq!(turn["role"], "assistant");
    assert_eq!(turn["truncated"], false, "an answer that fit was not cut");
    assert!(turn["error_code"].is_null(), "{turn}");
    assert!(turn["completed_at"].is_i64(), "{turn}");

    // The prompt reached the service verbatim, under the documented capability,
    // and a first turn carries no history.
    let seen = service.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].capability, AI_CHAT_CAPABILITY);
    assert_eq!(seen[0].payload["message"], "ikinci yasa nedir?");
    assert_eq!(seen[0].payload["history"], json!([]));
}

#[tokio::test]
async fn the_history_a_service_receives_is_oldest_first_without_the_new_turn() {
    // The contract a service in any language implements against: `history` is
    // the tail *before* this turn, oldest first, settled turns only — and the
    // new prompt rides in `message` alone, never duplicated into the history.
    //
    // The prior turn goes through the real API — both of its rows land in the
    // same millisecond, so this also pins the ordering tie-break. Only the
    // never-answered row is seeded: nothing in the API leaves one behind.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Reply("birinci cevap".into()),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = chat_app(&bridge).await;
    let (cookie, thread) = chat_user(&app, "ali").await;
    let user = common::me_id(&app, &cookie).await;

    let first = ask(&app, &cookie, &thread, "birinci soru").await;
    assert_eq!(
        settled(&app, &cookie, &thread, &first).await["status"],
        "complete"
    );

    // History is ordered by `created_at`, so this seeded row's 1002 timestamp
    // sorts it before everything the turns above minted — the never-answered
    // prior turn the new prompt must not duplicate into.
    sqlx::query(
        "INSERT INTO chatbot_message (id, thread_id, user_id, role, content, status, created_at) \
         VALUES ($1, $2, $3, 'assistant', '', 'pending', 1002)",
    )
    .bind(Uuid::now_v7())
    .bind(Uuid::parse_str(&thread).expect("thread id"))
    .bind(Uuid::parse_str(&user).expect("user id"))
    .execute(&db)
    .await
    .expect("seed history");

    let mid = ask(&app, &cookie, &thread, "ikinci soru").await;
    let turn = settled(&app, &cookie, &thread, &mid).await;
    assert_eq!(turn["status"], "complete", "{turn}");

    let seen = service.seen();
    assert_eq!(seen.len(), 2, "one dispatch per turn, never a re-send");
    assert_eq!(seen[1].payload["message"], "ikinci soru");
    assert_eq!(
        seen[1].payload["history"],
        json!([
            { "role": "user", "content": "birinci soru" },
            { "role": "assistant", "content": "birinci cevap" },
        ]),
        "oldest first, no new turn, no unsettled turn"
    );
}

#[tokio::test]
async fn a_refused_chat_turn_carries_the_services_own_error_code() {
    // A service may define codes this backend has never heard of; they reach
    // the browser verbatim so the UI can explain the real reason.
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Fail {
            code: "quota_exhausted".into(),
            message: "no credit left".into(),
        },
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, _db) = chat_app(&bridge).await;
    let (cookie, thread) = chat_user(&app, "ali").await;

    let mid = ask(&app, &cookie, &thread, "bir soru").await;
    let turn = settled(&app, &cookie, &thread, &mid).await;
    assert_eq!(turn["status"], "failed", "{turn}");
    assert_eq!(turn["error_code"], "quota_exhausted");
    assert_eq!(turn["content"], "", "a failed turn shows no text");
    assert_eq!(turn["truncated"], false, "and nothing to have been cut");
    assert!(turn["completed_at"].is_i64(), "{turn}");
}

#[tokio::test]
async fn an_over_long_chat_answer_is_clipped_rather_than_failed() {
    // The service is a trust boundary: an answer past the school's cap is
    // truncated (a clipped answer still helps), and on a character boundary —
    // a byte-wise clip of a multi-byte script would render as garbage. The cut
    // is flagged, so the UI never passes a clipped answer off as the whole one.
    let cap = DEFAULT_MAX_CHATBOT_MESSAGE_LEN as usize;
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Reply("é".repeat(cap + 500)),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, _db) = chat_app(&bridge).await;
    let (cookie, thread) = chat_user(&app, "ali").await;

    let mid = ask(&app, &cookie, &thread, "uzun cevap ver").await;
    let turn = settled(&app, &cookie, &thread, &mid).await;
    assert_eq!(turn["status"], "complete", "{turn}");
    let text = turn["content"].as_str().expect("content");
    assert_eq!(text.chars().count(), cap);
    assert!(text.chars().all(|c| c == 'é'), "clipped mid-character");
    assert_eq!(turn["truncated"], true, "the clip must be visible: {turn}");
}

#[tokio::test]
async fn a_blank_chat_answer_fails_as_empty_reply() {
    // A blank bubble is indistinguishable from a bug, so it is reported as one
    // rather than stored as an answer.
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Reply("   \n ".into()),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, _db) = chat_app(&bridge).await;
    let (cookie, thread) = chat_user(&app, "ali").await;

    let mid = ask(&app, &cookie, &thread, "bir soru").await;
    let turn = settled(&app, &cookie, &thread, &mid).await;
    assert_eq!(turn["status"], "failed", "{turn}");
    assert_eq!(turn["error_code"], "empty_reply");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_racing_turns_on_one_thread_each_answer_their_own_prompt() {
    // Two `POST .../messages` genuinely in flight on ONE thread. Their two
    // creates interleave, so the rows land `userA, userB, asstA, asstB` — and
    // any rule that resolves an answer's question by write order ("the newest
    // user row before this answer") then hands *both* answers prompt B and
    // never answers prompt A at all. The prompt travels by id precisely so
    // that ordering cannot decide it.
    //
    // The service echoes the prompt it was given back inside its reply, so a
    // crossed pair is legible in the stored row rather than inferred: the
    // answer to "soru A" would read `cevap::soru B`.
    //
    // The interleave is scheduled, not forced — nothing in the handler can be
    // held open between its two creates. So the round is replayed on a fresh
    // user/thread until the row order proves it happened, and a run that never
    // interleaves fails rather than passing vacuously.
    const ROUNDS: usize = 60;
    const ENOUGH: usize = 3;
    let bridge = bridge_with_timeout(Duration::from_secs(20)).await;
    let mut chatty = hello("tutor", &[AI_CHAT_CAPABILITY]);
    chatty.max_concurrent = Some(8);
    let _service = connect_service(&bridge, chatty, Behaviour::EchoPrompt).await;
    await_workers(&bridge, 1).await;
    let (app, _db) = chat_app(&bridge).await;

    let mut interleaved = 0usize;
    for round in 0..ROUNDS {
        let (cookie, thread) = chat_user(&app, &format!("ali{round}")).await;
        let asks = ["soru A", "soru B"].map(|text| {
            let (app, cookie, thread) = (app.clone(), cookie.clone(), thread.clone());
            tokio::spawn(async move { ask(&app, &cookie, &thread, text).await })
        });
        let mut mids = Vec::new();
        for handle in asks {
            mids.push(handle.await.expect("the POST task did not panic"));
        }

        // Both prompts appended before either answer row: the interleave the
        // bug needed. Ids are minted in write order, so the list is that order.
        let rows = thread_rows(&app, &cookie, &thread).await;
        let roles: Vec<String> = rows
            .iter()
            .map(|row| row["role"].as_str().expect("role").to_string())
            .collect();
        assert_eq!(roles.len(), 4, "two turns are four rows: {roles:?}");
        if roles == ["user", "user", "assistant", "assistant"] {
            interleaved += 1;
        }

        for (mid, asked) in mids.iter().zip(["soru A", "soru B"]) {
            let turn = settled(&app, &cookie, &thread, mid).await;
            assert_eq!(turn["status"], "complete", "round {round}: {turn}");
            assert_eq!(
                turn["content"],
                format!("cevap::{asked}"),
                "round {round}, rows {roles:?}: this answer was paired with the other request's prompt",
            );
        }

        // The other half of the bug: a prompt nothing ever answered left its
        // reserved row spinning. Nothing in the thread may stay `pending`.
        for row in thread_rows(&app, &cookie, &thread).await {
            assert_ne!(row["status"], "pending", "round {round}: {row}");
        }

        // A few proven races are the point; the rest of the budget only exists
        // for a machine that schedules them rarely.
        if interleaved == ENOUGH {
            break;
        }
    }
    assert!(
        interleaved > 0,
        "the two POSTs never interleaved in {ROUNDS} rounds — this run proved nothing \
         about prompt pairing; widen the race rather than trusting the pass",
    );
}

#[tokio::test]
async fn a_service_is_told_the_askers_own_school_role() {
    // The service answers differently for a student than for a manager, so the
    // role it reads must be the asker's own. Two different roles go through the
    // real endpoint: a hardcoded slug would pass one of them and fail the other.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Reply("peki".into()),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = chat_app(&bridge).await;

    for (name, role) in [("ali", "student"), ("veli", "manager")] {
        let cookie = common::login_as(&app, &db, name, role).await;
        let res = common::send(
            &app,
            "POST",
            "/chatbot/threads",
            Some(&cookie),
            Some(json!({})),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        let thread = common::id_of(&res.body);

        // The body tries to promote itself. It must change nothing: the role is
        // read from the session, and a send body carries only `content`.
        let res = common::send(
            &app,
            "POST",
            &format!("/chatbot/threads/{thread}/messages"),
            Some(&cookie),
            Some(json!({ "content": "soru", "asker_role": "admin" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
        let mid = res.body["message_id"].as_str().expect("message_id");
        assert_eq!(
            settled(&app, &cookie, &thread, mid).await["status"],
            "complete"
        );
    }

    // Read as untyped JSON, the way a service in another language reads it.
    let seen = service.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].payload["asker_role"], "student");
    assert_eq!(
        seen[1].payload["asker_role"], "manager",
        "the second turn was asked by a manager: {}",
        seen[1].payload
    );
    // And the author role on a turn is a different key, so neither can be read
    // for the other.
    assert!(seen[0].payload["history"].is_array());
}

/// Every row of a thread, oldest first.
async fn thread_rows(app: &Router, cookie: &str, thread: &str) -> Vec<Value> {
    let res = common::send(
        app,
        "GET",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    common::items(&res.body).clone()
}

// ------------------------------------------------------------- boot wiring --
//
// `ai::start_bridge` is the seam between deployment config and the listener.
// Its branches are the ones an operator hits, and the failure branch is the
// one that matters most: a set address with no token must refuse to boot
// rather than listen unauthenticated, since anyone able to reach the UDP port
// could otherwise register a worker and answer real users' messages.

/// A config with the AI fields under the test's control. Everything else comes
/// from the usual defaults — these tests never touch the database.
fn ai_config(addr: Option<&str>, token: Option<&str>) -> hezarfen_backend::config::Config {
    let mut cfg = hezarfen_backend::config::Config::from_env();
    cfg.ai_quic_addr = addr.map(str::to_string);
    cfg.ai_shared_token = token.map(str::to_string);
    cfg.ai_tls_cert = None;
    cfg.ai_tls_key = None;
    cfg
}

#[tokio::test]
async fn no_address_means_no_bridge_not_a_failed_boot() {
    // The school API predates the AI features and must still boot without
    // them, so "unconfigured" is a supported deployment, not a degraded one.
    let bridge = hezarfen_backend::ai::start_bridge(&ai_config(None, None))
        .await
        .unwrap_or_else(|e| panic!("an unconfigured bridge is not an error: {e}"));
    assert!(bridge.is_none(), "nothing should be listening");
}

#[tokio::test]
async fn an_address_without_a_token_refuses_to_boot() {
    // The security branch: never fall back to an unauthenticated listener.
    //
    // Two layers refuse this — `start_bridge` on the absent env var, and
    // `AiBridge::bind` on an empty token — so the assertion pins the message
    // to *this* layer's wording (`AI_QUIC_ADDR`). Asserting only on
    // "AI_SHARED_TOKEN" would match either, and would still pass with this
    // branch deleted.
    let Err(err) = hezarfen_backend::ai::start_bridge(&ai_config(Some("127.0.0.1:0"), None)).await
    else {
        panic!("a tokenless bridge must not start");
    };
    assert!(
        matches!(&err, AiError::Setup(m) if m.contains("AI_SHARED_TOKEN") && m.contains("AI_QUIC_ADDR")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_bad_address_fails_the_boot_rather_than_going_quiet() {
    // The operator asked for AI; a typo must not degrade to a silently dead
    // feature they only notice when a student sends the first message.
    let Err(err) =
        hezarfen_backend::ai::start_bridge(&ai_config(Some("not-an-address"), Some(TOKEN))).await
    else {
        panic!("a malformed address must not be ignored");
    };
    assert!(
        matches!(&err, AiError::Setup(m) if m.contains("AI_QUIC_ADDR")),
        "got {err:?}"
    );
}

// --------------------------------------------------------------- api read --
//
// The reverse direction: a registered service opens its own stream and reads
// the school API. These drive the real router over real QUIC, so what they
// assert is what a service actually receives — including who it is allowed to
// be, which is the security half of the feature.

use hezarfen_backend::ai::protocol::{ApiRequest, ApiResponse};

/// One api read on a fresh client-initiated stream.
async fn api_read(conn: &quinn::Connection, request: ApiRequest) -> ApiResponse {
    let (mut send, mut recv) = conn.open_bi().await.expect("api-read stream");
    write_frame(&mut send, &request)
        .await
        .expect("write ApiRequest");
    let _ = send.finish();
    read_frame(&mut recv).await.expect("read ApiResponse")
}

/// A `GET` of `path` as `on_behalf_of` (or as the service itself).
fn read_of(path: &str, on_behalf_of: Option<&str>) -> ApiRequest {
    read_of_school(DEMO_SLUG, path, on_behalf_of)
}

/// The same, naming the school explicitly — the tenancy tests below.
fn read_of_school(school: &str, path: &str, on_behalf_of: Option<&str>) -> ApiRequest {
    ApiRequest {
        id: format!("trace-{path}"),
        school: school.to_string(),
        path: path.to_string(),
        query: None,
        on_behalf_of: on_behalf_of.map(str::to_string),
        method: None,
    }
}

/// Unwrap an `Ok` answer into (status, body); panics on a refusal, naming it.
fn ok_answer(answer: ApiResponse) -> (u16, Value) {
    match answer {
        ApiResponse::Ok { status, body, .. } => (status, body),
        ApiResponse::Err { code, message, .. } => {
            panic!("expected the api to answer, got refusal {code}: {message}")
        }
    }
}

/// A registered service plus a router armed for its reads, plus a seeded
/// student (id and session cookie).
async fn api_service(bridge: &AiBridge) -> (FakeService, Router, String, String) {
    let service = connect_service(
        bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;
    let cookie = common::login_as(&app, &db, "ayse", "student").await;
    let student = common::me_id(&app, &cookie).await;
    (service, app, student, cookie)
}

#[tokio::test]
async fn a_service_reading_on_behalf_of_a_student_gets_that_students_own_answer() {
    // The entry surface, end to end: what the service reads over QUIC must be
    // byte-for-byte the JSON that student's own browser gets from REST. A
    // second-hand answer (the service's own principal, a stale row) would
    // differ here rather than in whatever the model says weeks later.
    let bridge = bridge().await;
    let (service, app, student, cookie) = api_service(&bridge).await;

    let rest = common::send(&app, "GET", "/auth/me", Some(&cookie), None).await;
    assert_eq!(rest.status, StatusCode::OK);

    let (status, body) =
        ok_answer(api_read(&service.conn, read_of("/auth/me", Some(&student))).await);
    assert_eq!(status, 200);
    assert_eq!(body, rest.body, "the service read a different /auth/me");
}

#[tokio::test]
async fn a_service_reading_as_itself_is_the_ai_principal() {
    // With nobody named, the request runs as the synthetic `ai` principal.
    // That principal is authenticated (the extension satisfies the extractor),
    // so an own-scoped read succeeds and returns *its* empty data — while a
    // role-gated read is a 403, not a 401: the caller is known, just below
    // every human role.
    let bridge = bridge().await;
    let (service, _app, student, _cookie) = api_service(&bridge).await;

    let (status, body) = ok_answer(api_read(&service.conn, read_of("/notes", None)).await);
    assert_eq!(status, 200);
    assert_eq!(
        common::items(&body).len(),
        0,
        "the ai principal owns nothing"
    );

    let (status, _) =
        ok_answer(api_read(&service.conn, read_of(&format!("/marks/{student}"), None)).await);
    assert_eq!(
        status, 403,
        "reading another person's marks needs teacher+ or a parent link"
    );
}

#[tokio::test]
async fn a_router_status_rides_back_as_an_ok_answer_not_a_refusal() {
    // The `Err` frame is reserved for bridge refusals. Anything the API itself
    // answered — including a 404 — is an `Ok` carrying that status, so a
    // service can tell "I was not allowed to ask" from "I asked and this is
    // the answer".
    let bridge = bridge().await;
    let (service, _app, _student, _cookie) = api_service(&bridge).await;

    let (status, _) = ok_answer(api_read(&service.conn, read_of("/notes/nosuchnote", None)).await);
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_down_database_socket_is_refused_as_unavailable_rather_than_parked() {
    // This path skips the HTTP layers, so the bridge resolves the school's
    // registry row itself, on the control database, before every read. Take
    // that table away and the read is refused as `unavailable` — promptly,
    // never parked waiting on a store nobody is running — instead of hanging
    // out the service's own deadline.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (_app, _db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;
    let control = tenants.control();

    sqlx::query("ALTER TABLE school RENAME TO school_out")
        .execute(control)
        .await
        .expect("take the registry away");
    match api_read(&service.conn, read_of("/notes", None)).await {
        ApiResponse::Err { code, id, .. } => {
            assert_eq!(code, "unavailable");
            assert_eq!(id, "trace-/notes", "the trace id comes back");
        }
        other => panic!("a read against a down database must be refused: {other:?}"),
    }

    // And that refusal is the outage's doing, not a broken bridge: the same
    // read on the same stream-opening service answers once the store is back.
    sqlx::query("ALTER TABLE school_out RENAME TO school")
        .execute(control)
        .await
        .expect("give the registry back");
    let (status, _) = ok_answer(api_read(&service.conn, read_of("/notes", None)).await);
    assert_eq!(status, 200);
}

#[tokio::test]
async fn a_dead_user_id_is_refused_rather_than_run_as_somebody() {
    // The principal is loaded live, so a service holding an id of a user who
    // has since been deleted is told so — never silently downgraded to the ai
    // principal, which would answer a question about the wrong person.
    let bridge = bridge().await;
    let (service, _app, _student, _cookie) = api_service(&bridge).await;

    match api_read(&service.conn, read_of("/auth/me", Some("ghost"))).await {
        ApiResponse::Err { code, id, .. } => {
            assert_eq!(code, "unknown_user");
            assert_eq!(id, "trace-/auth/me", "the trace id comes back");
        }
        other => panic!("a dead id must not be dispatched: {other:?}"),
    }
}

#[tokio::test]
async fn the_record_form_of_a_user_id_reads_the_same_as_the_bare_key() {
    // The protocol doc spells the id `user:<key>`, REST paths spell it bare.
    // Both are the same person; accepting only one would make the contract a
    // trap for the first service that copies the doc example.
    let bridge = bridge().await;
    let (service, _app, student, _cookie) = api_service(&bridge).await;

    let bare = ok_answer(api_read(&service.conn, read_of("/auth/me", Some(&student))).await);
    let prefixed = ok_answer(
        api_read(
            &service.conn,
            read_of("/auth/me", Some(&format!("user:{student}"))),
        )
        .await,
    );
    assert_eq!(bare, prefixed);
}

#[tokio::test]
async fn the_query_field_reaches_the_route_as_a_real_query_string() {
    // Paging is how a service reads a long list without blowing the frame cap,
    // so `query` has to arrive at the handler rather than being dropped: two
    // notes exist, `limit=1` must return one of them and still report the total.
    let bridge = bridge().await;
    let (service, app, student, cookie) = api_service(&bridge).await;
    for title in ["ilk not", "ikinci not"] {
        let res = common::send(
            &app,
            "POST",
            "/notes",
            Some(&cookie),
            Some(json!({ "title": title })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    let mut request = read_of("/notes", Some(&student));
    request.query = Some("limit=1&offset=0".to_string());
    let (status, body) = ok_answer(api_read(&service.conn, request).await);
    assert_eq!(status, 200);
    assert_eq!(
        common::items(&body).len(),
        1,
        "limit=1 was honoured: {body}"
    );
    assert_eq!(common::total(&body), 2, "both notes were counted: {body}");
}

#[tokio::test]
async fn the_ai_principal_cannot_be_forged_over_http() {
    // The whole reason the principal travels as a request *extension*: nothing
    // arriving on the public HTTP port can set one. A caller with no cookie
    // stays unauthenticated however it names the header.
    use tower::ServiceExt;
    let bridge = bridge().await;
    let (_service, app, student, _cookie) = api_service(&bridge).await;

    let spoofs = [
        ("ai-principal", student.as_str()),
        ("x-ai-principal", student.as_str()),
        ("aiprincipal", student.as_str()),
        ("on-behalf-of", student.as_str()),
    ];
    for (header, value) in spoofs {
        let request = axum::http::Request::builder()
            .uri("/auth/me")
            .header(header, value)
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.expect("router answered");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "header `{header}` bought a session"
        );
    }
    // And a cookie-less read of the same path is 401 with no header at all.
    let res = common::send(&app, "GET", "/auth/me", None, None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_configured_bridge_listens_and_serves_a_real_handshake() {
    // The whole point of the wiring: what `start_bridge` returns is a live
    // listener, not just a constructed value — so dial it for real.
    let bridge = hezarfen_backend::ai::start_bridge(&ai_config(Some("127.0.0.1:0"), Some(TOKEN)))
        .await
        .unwrap_or_else(|e| panic!("a fully configured bridge starts: {e}"))
        .expect("a configured bridge is Some");
    let _service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(&bridge, 1).await;
    assert_eq!(bridge.workers().len(), 1);
}

// ------------------------------------------------------- course-note rag --
//
// The `rag.index` dispatch end to end: a real course-note handler, a real QUIC
// round trip, and a fake service standing in for the indexer. These pin the
// three rules the feature rests on — the handler never waits on the service,
// no service means no rows and no failure, and a deleted note takes its
// outputs with it.

use hezarfen_backend::constant::AI_RAG_INDEX_CAPABILITY;
use hezarfen_backend::domain::course_note::CourseNoteId;
use hezarfen_backend::domain::rag_output::RagOutput;

/// Every stored output of `note`, newest first.
async fn outputs(db: &Database, note: &str) -> Vec<RagOutput> {
    hezarfen_backend::db::rag_output::list_for(db, &CourseNoteId::from_key(note), None, 0)
        .await
        .expect("list rag outputs")
        .0
}

/// Poll until `note` has stored outputs. Bounded polling rather than a sleep
/// sized to the dispatch: it is fire-and-forget, so it lands when it lands.
async fn await_outputs(db: &Database, note: &str) -> Vec<RagOutput> {
    for _ in 0..500 {
        let stored = outputs(db, note).await;
        if !stored.is_empty() {
            return stored;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no rag_output row ever landed for course note {note}");
}

/// A teacher with a course and one note on it: (cookie, note id).
async fn course_note(app: &Router, db: &Database) -> (String, String) {
    let cookie = common::login_as(app, db, "ogretmen", "teacher").await;
    let course = common::create_course(app, &cookie, "fizik").await;
    let res = common::send(
        app,
        "POST",
        "/course-notes",
        Some(&cookie),
        Some(json!({ "course": course, "title": "Bölüm 3", "content": "özet" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    (cookie, common::id_of(&res.body))
}

#[tokio::test]
async fn an_index_dispatch_names_the_notes_own_school_and_stores_the_answer_there() {
    // The outbound half of `hab/2`, end to end and across two schools: the
    // frame the service receives names the school whose teacher wrote the note,
    // and the answer is stored in *that* school's database — not the control
    // one, and not the neighbour's. A bridge that carried no school could only
    // have guessed, and this is the test that would catch the guess.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("indexer", &[AI_RAG_INDEX_CAPABILITY]),
        Behaviour::Answer(json!({ "summary": "beta" })),
    )
    .await;
    await_workers(&bridge, 1).await;

    let (app, demo_db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;
    let beta = Slug::try_new("beta").unwrap();
    let beta_db = tenants
        .create(
            hezarfen_backend::tenant::SchoolId::generate(),
            &beta,
            "Beta College",
            ModuleSet::all(),
        )
        .await
        .expect("beta");

    let cookie = common::login_as_school(&app, &beta_db, "beta", "ogretmen", "teacher").await;
    let course = common::create_course(&app, &cookie, "fizik").await;
    let res = common::send(
        &app,
        "POST",
        "/course-notes",
        Some(&cookie),
        Some(json!({ "course": course, "title": "Bölüm 3", "content": "özet" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let note = common::id_of(&res.body);

    let stored = await_outputs(&beta_db, &note).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].get_payload()["summary"], "beta");
    assert!(
        outputs(&demo_db, &note).await.is_empty(),
        "the neighbouring school must hold no row for beta's note"
    );

    let seen = service.seen();
    assert_eq!(seen[0].school, "beta", "the frame named the wrong school");
    assert_eq!(seen[0].payload["course_note"], note);
}

#[tokio::test]
async fn a_course_note_is_indexed_and_its_output_stored_with_its_sources() {
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("indexer", &[AI_RAG_INDEX_CAPABILITY]),
        Behaviour::Answer(json!({ "summary": "x" })),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;

    let (cookie, note) = course_note(&app, &db).await;
    let stored = await_outputs(&db, &note).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].get_payload()["summary"], "x");
    assert!(stored[0].get_sources().is_empty(), "the note had no files");

    // The service saw the note itself, under the documented capability.
    let seen = service.seen();
    assert_eq!(seen[0].capability, AI_RAG_INDEX_CAPABILITY);
    assert_eq!(
        seen[0].school, DEMO_SLUG,
        "the frame names the caller's school"
    );
    assert_eq!(seen[0].payload["course_note"], note);
    assert_eq!(seen[0].payload["title"], "Bölüm 3");
    assert_eq!(seen[0].payload["content"], "özet");
    assert_eq!(seen[0].payload["files"], json!([]));

    // Attaching a file re-indexes, and the fresh output cites it — one row,
    // not two: a re-index replaces, it does not accumulate.
    let file = common::upload_course_note_file(
        &app,
        &cookie,
        &note,
        "recap.pdf",
        "application/pdf",
        b"pdf bytes",
    )
    .await;
    assert_eq!(file.status, StatusCode::CREATED, "{}", file.body);
    let file_id = common::id_of(&file.body);
    for _ in 0..500 {
        let stored = outputs(&db, &note).await;
        if stored.len() == 1 && stored[0].get_sources().len() == 1 {
            assert_eq!(stored[0].get_sources()[0].key(), file_id);
            // File CONTENT is deliberately not on the wire — metadata only.
            let seen = service.seen();
            let last = &seen[seen.len() - 1].payload["files"][0];
            assert_eq!(last["id"], file_id);
            assert_eq!(last["name"], "recap.pdf");
            assert_eq!(last["size"], 9);
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the upload never re-indexed: {:?}",
        outputs(&db, &note).await
    );
}

#[tokio::test]
async fn with_no_indexing_service_a_note_is_still_created_and_stores_nothing() {
    // A worker is connected, just not one carrying `rag.index` — the trigger
    // is a silent no-op, never a failed or slowed 201.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;

    let (_, note) = course_note(&app, &db).await;
    // Long enough that a dispatch would have landed (the chat round trips in
    // this suite settle in milliseconds).
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(outputs(&db, &note).await.is_empty());
    assert!(service.seen().is_empty(), "nothing was dispatched");
}

#[tokio::test]
async fn deleting_a_course_note_takes_its_rag_outputs_with_it() {
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("indexer", &[AI_RAG_INDEX_CAPABILITY]),
        Behaviour::Answer(json!({ "summary": "x" })),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;

    let (cookie, note) = course_note(&app, &db).await;
    await_outputs(&db, &note).await;

    let res = common::send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert!(outputs(&db, &note).await.is_empty());
}

// ------------------------------------------------------- course notes read --

/// A registered service, a router, and a course the seeded student is
/// enrolled in (plus one they are not) with a teacher's note in each.
/// Returns (service, student id, note id, enrolled course, foreign course).
async fn course_notes_fixture(
    bridge: &AiBridge,
) -> (
    FakeService,
    String,
    String,
    String,
    String,
    Router,
    Database,
    String,
) {
    let service = connect_service(
        bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;

    let student_cookie = common::login_as(&app, &db, "ayse", "student").await;
    let student = common::me_id(&app, &student_cookie).await;
    let teacher = common::login_as(&app, &db, "hoca", "teacher").await;

    let course = common::create_course(&app, &teacher, "Physics").await;
    common::enroll(&app, &teacher, &course, &student).await;
    let foreign = common::create_course(&app, &teacher, "Chemistry").await;

    let res = common::send(
        &app,
        "POST",
        "/course-notes",
        Some(&teacher),
        Some(json!({ "course": course, "title": "Newton", "content": "F = ma" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "teacher creates the note");
    let note = common::id_of(&res.body);

    (service, student, note, course, foreign, app, db, teacher)
}

#[tokio::test]
async fn a_service_reads_a_course_note_on_behalf_of_an_enrolled_student() {
    // The point of #27: a study companion answering about a lesson needs the
    // teacher's own material, read with the student's reach and no wider.
    let bridge = bridge().await;
    let (service, student, note, course, _foreign, _app, _db, _teacher) =
        course_notes_fixture(&bridge).await;

    let request = ApiRequest {
        id: "trace-course-notes".into(),
        school: DEMO_SLUG.into(),
        path: "/course-notes".into(),
        query: Some(format!("course={course}")),
        on_behalf_of: Some(student),
        method: None,
    };
    let (status, body) = ok_answer(api_read(&service.conn, request).await);
    assert_eq!(status, 200);
    let items = common::items(&body);
    assert_eq!(
        items.len(),
        1,
        "the enrolled student sees the course's note"
    );
    assert_eq!(items[0]["id"], note);
    assert_eq!(items[0]["title"], "Newton");
}

#[tokio::test]
async fn a_course_the_student_is_not_in_is_refused_by_the_handler() {
    // The bridge widens the scope, never the reach: the handler's own guard
    // is what answers, and it rides back as an `Ok` carrying that status.
    let bridge = bridge().await;
    let (service, student, _note, _course, foreign, _app, _db, _teacher) =
        course_notes_fixture(&bridge).await;

    let request = ApiRequest {
        id: "trace-foreign-course".into(),
        school: DEMO_SLUG.into(),
        path: "/course-notes".into(),
        query: Some(format!("course={foreign}")),
        on_behalf_of: Some(student),
        method: None,
    };
    let (status, _) = ok_answer(api_read(&service.conn, request).await);
    assert_eq!(status, 403, "not enrolled: the course-view guard forbids");
}

#[tokio::test]
async fn writing_a_course_note_is_refused_before_dispatch() {
    // Read scope means read: the allowlist admits the path, the method gate
    // still refuses, and nothing reaches the router.
    let bridge = bridge().await;
    let (service, _student, _note, _course, _foreign, _app, _db, _teacher) =
        course_notes_fixture(&bridge).await;

    let request = ApiRequest {
        id: "trace-post".into(),
        school: DEMO_SLUG.into(),
        path: "/course-notes".into(),
        query: None,
        on_behalf_of: None,
        method: Some("POST".into()),
    };
    match api_read(&service.conn, request).await {
        ApiResponse::Err { code, .. } => assert_eq!(code, "method_not_allowed"),
        ApiResponse::Ok { status, .. } => panic!("a POST was dispatched, answering {status}"),
    }
}

#[tokio::test]
async fn a_course_note_file_download_stays_out_of_the_read_scope() {
    // The listing of a note's files is JSON and allowed; the bytes behind one
    // are not — frames carry JSON under an 8 MiB cap.
    let bridge = bridge().await;
    let (service, student, note, _course, _foreign, _app, _db, _teacher) =
        course_notes_fixture(&bridge).await;

    let path = format!("/course-notes/{note}/files/somefile");
    match api_read(&service.conn, read_of(&path, Some(&student))).await {
        ApiResponse::Err { code, .. } => assert_eq!(code, "path_not_allowed"),
        ApiResponse::Ok { status, .. } => panic!("a blob route was dispatched, answering {status}"),
    }
}

// -------------------------------------------------------------- blob read --
//
// The third stream shape: a service pulls a course-note attachment's raw
// bytes, which no JSON frame could carry. The header frame is authorized by
// the very guard the HTTP download carries, so these drive the real upload
// route and then read the bytes back over real QUIC.

use hezarfen_backend::ai::protocol::{BlobRequest, BlobResponse};
use hezarfen_backend::constant::AI_BLOB_WRITE_STALL_SECS;

/// `len` bytes of deterministic pseudo-random data. The default 200 KiB is
/// well past one QUIC datagram, so a passing read proves the copy really
/// streamed rather than fitting in a single write.
fn blob_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// One blob read on a fresh client-initiated stream: the header frame, then
/// everything that followed it up to EOF.
async fn blob_read(conn: &quinn::Connection, request: BlobRequest) -> (BlobResponse, Vec<u8>) {
    let (mut send, mut recv) = conn.open_bi().await.expect("blob stream");
    write_frame(&mut send, &request)
        .await
        .expect("write BlobRequest");
    let _ = send.finish();
    let header: BlobResponse = read_frame(&mut recv).await.expect("read BlobResponse");
    let bytes = recv
        .read_to_end(16 * 1024 * 1024)
        .await
        .expect("read the blob body to EOF");
    (header, bytes)
}

fn blob_of(file: &str, on_behalf_of: Option<&str>) -> BlobRequest {
    blob_of_school(DEMO_SLUG, file, on_behalf_of)
}

/// The same, naming the school explicitly.
fn blob_of_school(school: &str, file: &str, on_behalf_of: Option<&str>) -> BlobRequest {
    BlobRequest {
        id: format!("trace-blob-{file}"),
        school: school.to_string(),
        file: file.to_string(),
        on_behalf_of: on_behalf_of.map(str::to_string),
    }
}

/// The refusal code of a header that must be one; panics on an `Ok`.
fn blob_refusal(header: BlobResponse) -> String {
    match header {
        BlobResponse::Err { code, .. } => code,
        BlobResponse::Ok { name, size, .. } => {
            panic!("expected a refusal, got {size} bytes of `{name}`")
        }
    }
}

#[tokio::test]
async fn a_service_streams_a_course_note_file_on_behalf_of_an_enrolled_student() {
    // The whole point of the slice: the service can index the PDF, not just
    // its filename. The bytes must come back identical to what was uploaded
    // through the ordinary multipart route, and `size` must be exactly how
    // many of them arrive — a service reads that count and then expects EOF.
    let bridge = bridge().await;
    let (service, student, note, _course, _foreign, app, _db, teacher) =
        course_notes_fixture(&bridge).await;

    let uploaded = blob_bytes(200 * 1024);
    let res = common::upload_file_at(
        &app,
        &teacher,
        &format!("/course-notes/{note}/files"),
        "recap.pdf",
        "application/pdf",
        &uploaded,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let file = common::id_of(&res.body);

    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&student))).await;
    let BlobResponse::Ok {
        id,
        school,
        name,
        content_type,
        size,
    } = header
    else {
        panic!("the enrolled student was refused: {}", blob_refusal(header));
    };
    assert_eq!(id, format!("trace-blob-{file}"), "the trace id is echoed");
    assert_eq!(school, DEMO_SLUG, "the school is echoed too");
    assert_eq!(name, "recap.pdf");
    assert_eq!(content_type, "application/pdf");
    assert_eq!(size as usize, uploaded.len(), "the promised byte count");
    assert_eq!(bytes.len(), size as usize, "exactly `size` bytes, then FIN");
    assert!(bytes == uploaded, "the bytes differ from what was uploaded");
}

#[tokio::test]
async fn a_student_outside_the_course_is_refused_the_bytes() {
    // The bridge widens who may ask, never what may be read: the file's own
    // course-view guard answers, exactly as it does over HTTP.
    let bridge = bridge().await;
    let (service, _student, note, _course, _foreign, app, db, teacher) =
        course_notes_fixture(&bridge).await;

    let res = common::upload_file_at(
        &app,
        &teacher,
        &format!("/course-notes/{note}/files"),
        "recap.pdf",
        "application/pdf",
        b"gizli",
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let file = common::id_of(&res.body);

    let outsider_cookie = common::login_as(&app, &db, "veli", "student").await;
    let outsider = common::me_id(&app, &outsider_cookie).await;
    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&outsider))).await;
    assert_eq!(blob_refusal(header), "forbidden");
    assert!(bytes.is_empty(), "a refusal is followed by nothing at all");
}

#[tokio::test]
async fn a_service_reading_the_bytes_as_itself_is_forbidden() {
    // Without `on_behalf_of` the principal is the `ai` role, which is enrolled
    // in nothing and manages nothing — so it can view no course, and the blob
    // stream grants it no reach the api read would not.
    let bridge = bridge().await;
    let (service, _student, note, _course, _foreign, app, _db, teacher) =
        course_notes_fixture(&bridge).await;

    let res = common::upload_file_at(
        &app,
        &teacher,
        &format!("/course-notes/{note}/files"),
        "recap.pdf",
        "application/pdf",
        b"gizli",
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let file = common::id_of(&res.body);

    let (header, _) = blob_read(&service.conn, blob_of(&file, None)).await;
    assert_eq!(blob_refusal(header), "forbidden");
}

#[tokio::test]
async fn an_unknown_file_key_is_not_found_rather_than_a_dropped_stream() {
    let bridge = bridge().await;
    let (service, student, _note, _course, _foreign, _app, _db, _teacher) =
        course_notes_fixture(&bridge).await;

    let (header, _) = blob_read(&service.conn, blob_of("01NOSUCHFILE", Some(&student))).await;
    assert_eq!(blob_refusal(header), "not_found");
}

#[tokio::test]
async fn a_personal_notes_file_is_invisible_to_the_blob_stream() {
    // Scope is course-note attachments and nothing else. A personal note has
    // no reader but its owner, and the id is looked up in `course_note_file`
    // alone — so the owner's own id does not open it either: `not_found`, not
    // `forbidden`, because no such course-note file exists.
    let bridge = bridge().await;
    let (service, student, _note, _course, _foreign, app, db, _teacher) =
        course_notes_fixture(&bridge).await;

    let owner = common::login_as(&app, &db, "kemal", "student").await;
    let owner_id = common::me_id(&app, &owner).await;
    let res = common::send(
        &app,
        "POST",
        "/notes",
        Some(&owner),
        Some(json!({ "title": "özel", "content": "kimseye yok" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let personal = common::id_of(&res.body);
    let res = common::upload_file(
        &app,
        &owner,
        &personal,
        "gizli.pdf",
        "application/pdf",
        b"ozel",
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let personal_file = common::id_of(&res.body);

    for who in [&student, &owner_id] {
        let (header, _) = blob_read(&service.conn, blob_of(&personal_file, Some(who))).await;
        assert_eq!(
            blob_refusal(header),
            "not_found",
            "a personal note's file must not be reachable as a course-note file"
        );
    }
}

#[tokio::test]
async fn a_body_nobody_reads_is_reset_rather_than_left_streaming_forever() {
    // A service that opens a blob stream and then never reads it used to park
    // the copy task and an open file descriptor for the connection's whole
    // life: past the QUIC stream window nothing drains and the copy had no
    // deadline. The bound is per write, so this is the only shape it cuts — a
    // slow-but-reading service keeps earning fresh time.
    //
    // Spends ~AI_BLOB_WRITE_STALL_SECS of wall clock by construction: the
    // stall itself is what is under test, so there is nothing to poll for.
    let bridge = bridge().await;
    let (service, student, note, _course, _foreign, app, _db, teacher) =
        course_notes_fixture(&bridge).await;

    // Past quinn's default 1.25 MiB stream receive window and under the
    // default `max_file_bytes` (5 MiB), so the server blocks mid-body.
    let uploaded = blob_bytes(3 * 1024 * 1024);
    let res = common::upload_file_at(
        &app,
        &teacher,
        &format!("/course-notes/{note}/files"),
        "big.pdf",
        "application/pdf",
        &uploaded,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let file = common::id_of(&res.body);

    let (mut send, mut recv) = service.conn.open_bi().await.expect("blob stream");
    write_frame(&mut send, &blob_of(&file, Some(&student)))
        .await
        .expect("write BlobRequest");
    let _ = send.finish();
    let header: BlobResponse = read_frame(&mut recv).await.expect("read BlobResponse");
    assert!(
        matches!(header, BlobResponse::Ok { .. }),
        "the read was authorized: {}",
        blob_refusal(header)
    );

    // Nothing is read off `recv` until well past the stall bound.
    tokio::time::sleep(Duration::from_secs(AI_BLOB_WRITE_STALL_SECS + 3)).await;
    let err = tokio::time::timeout(Duration::from_secs(5), recv.read_to_end(8 * 1024 * 1024))
        .await
        .expect("the stream must already be resolved, not still parked")
        .expect_err("a stalled body must never FIN");
    assert!(
        matches!(err, quinn::ReadToEndError::Read(quinn::ReadError::Reset(_))),
        "expected a reset, got {err:?}"
    );
}

// ------------------------------------------------------ module entitlements --
//
// A school that never bought a module must be refused on both halves of the
// bridge: the api read, where the router's `route_layer` gate answers and the
// refusal rides back as an ordinary `Ok` status, and the blob stream, which
// bypasses the router entirely and checks `Module::CourseNotes` by hand.

use hezarfen_backend::module::Module;
use hezarfen_backend::tenant::Tenants;

/// Take `off` away from the demo school. Entitlements are read off the registry
/// row per request, so this lands on the very next frame — no reconnect, no
/// cached handle to go stale.
async fn demo_without(tenants: &Tenants, off: &[Module]) {
    let mut modules = ModuleSet::all();
    for module in off {
        modules.remove(*module);
    }
    modules.validate().expect("the narrowed set is satisfiable");
    tenants
        .set_modules(&demo(), &modules)
        .await
        .expect("narrow the demo school");
}

/// The body `AppError::ModuleDisabled` renders — the same JSON a browser gets.
fn module_disabled_body(module: Module) -> Value {
    json!({ "error": "module disabled", "module": module.as_str() })
}

#[tokio::test]
async fn an_api_read_of_a_disabled_module_is_the_gates_own_403_not_a_transport_refusal() {
    // The gate is a router layer and the bridge dispatches through the router,
    // so the entitlement holds on this seam for free — but only if the tenant
    // the dispatch carries is the real one. A service must see the school's own
    // 403, not `unavailable` and not a dropped stream; and two unrelated
    // modules prove the gate is generic rather than a special case for one.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("tutor", &[AI_CHAT_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;
    let cookie = common::login_as(&app, &db, "ayse", "student").await;
    let student = common::me_id(&app, &cookie).await;

    for (module, path) in [(Module::Notes, "/notes"), (Module::Homework, "/homework")] {
        let request = read_of(path, Some(&student));
        let (status, _) = ok_answer(api_read(&service.conn, request.clone()).await);
        assert_eq!(status, 200, "{path} answers while `{module}` is on");

        demo_without(&tenants, &[module]).await;
        let (status, body) = ok_answer(api_read(&service.conn, request.clone()).await);
        assert_eq!(status, 403, "{path} with `{module}` off: {body}");
        assert_eq!(body, module_disabled_body(module), "{path}");

        // Sold back, and the very next frame answers again: the entitlement is
        // the only thing that changed.
        demo_without(&tenants, &[]).await;
        let (status, _) = ok_answer(api_read(&service.conn, request).await);
        assert_eq!(status, 200, "{path} answers once `{module}` is back");
    }
}

/// A school with a course note and one attachment, plus the registry behind it:
/// (service, student id, file id, uploaded bytes, tenants).
async fn blob_fixture(bridge: &AiBridge) -> (FakeService, String, String, Vec<u8>, Tenants) {
    let service = connect_service(
        bridge,
        hello("indexer", &[AI_RAG_INDEX_CAPABILITY]),
        Behaviour::Echo,
    )
    .await;
    await_workers(bridge, 1).await;
    let (app, db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;

    let student_cookie = common::login_as(&app, &db, "ayse", "student").await;
    let student = common::me_id(&app, &student_cookie).await;
    let teacher = common::login_as(&app, &db, "hoca", "teacher").await;
    let course = common::create_course(&app, &teacher, "Physics").await;
    common::enroll(&app, &teacher, &course, &student).await;

    let res = common::send(
        &app,
        "POST",
        "/course-notes",
        Some(&teacher),
        Some(json!({ "course": course, "title": "Newton", "content": "F = ma" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let note = common::id_of(&res.body);

    let uploaded = blob_bytes(64 * 1024);
    let res = common::upload_file_at(
        &app,
        &teacher,
        &format!("/course-notes/{note}/files"),
        "recap.pdf",
        "application/pdf",
        &uploaded,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    (
        service,
        student,
        common::id_of(&res.body),
        uploaded,
        tenants,
    )
}

#[tokio::test]
async fn a_blob_read_is_refused_module_disabled_when_the_school_has_no_course_notes() {
    // The one surface the router's gate cannot reach. A refusal here must be
    // the header frame and *nothing after it*: a service reads `size` bytes on
    // an `Ok`, so a refusal followed by bytes would be read as a file.
    let bridge = bridge().await;
    let (service, student, file, uploaded, tenants) = blob_fixture(&bridge).await;

    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&student))).await;
    assert!(
        matches!(header, BlobResponse::Ok { .. }),
        "the module is on: {}",
        blob_refusal(header)
    );
    assert_eq!(bytes.len(), uploaded.len());

    demo_without(&tenants, &[Module::CourseNotes]).await;
    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&student))).await;
    assert_eq!(blob_refusal(header), "module_disabled");
    assert!(bytes.is_empty(), "a refusal is followed by nothing at all");

    // Sold back: the same request streams the same bytes as before.
    demo_without(&tenants, &[]).await;
    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&student))).await;
    assert!(
        matches!(header, BlobResponse::Ok { .. }),
        "the module is back: {}",
        blob_refusal(header)
    );
    assert!(bytes == uploaded, "the bytes differ from what was uploaded");
}

#[test]
fn course_notes_without_courses_is_unsellable_so_the_blob_path_never_sees_it() {
    // The blob stream checks `course_notes` alone. That is enough *because* the
    // set it reads can never hold `course_notes` without `courses` — the
    // dependency is refused before a school is ever narrowed to it, so nobody
    // has to check the parent module on this path.
    let mut broken = ModuleSet::all();
    broken.remove(Module::Courses);
    let err = broken
        .validate()
        .expect_err("course_notes cannot stand without courses");
    assert!(
        err.to_string()
            .contains("course_notes requires courses, which is not enabled"),
        "{err}"
    );
}

#[tokio::test]
async fn a_school_without_course_notes_cannot_reach_the_rag_dispatch_at_all() {
    // The write-back of `rag_output` has exactly four triggers, all of them
    // `/course-notes` handlers (`spawn_index` in `web::course_notes`), so the
    // nest's gate is the whole answer: no create, no dispatch, no row. This
    // pins that there is no second, ungated way in.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("indexer", &[AI_RAG_INDEX_CAPABILITY]),
        Behaviour::Answer(json!({ "summary": "x" })),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;

    let teacher = common::login_as(&app, &db, "ogretmen", "teacher").await;
    let course = common::create_course(&app, &teacher, "fizik").await;
    demo_without(&tenants, &[Module::CourseNotes]).await;

    let res = common::send(
        &app,
        "POST",
        "/course-notes",
        Some(&teacher),
        Some(json!({ "course": course, "title": "Bölüm 3", "content": "özet" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(res.body, module_disabled_body(Module::CourseNotes));

    // Nothing was sent to the indexer. The dispatch is fire-and-forget, so give
    // a frame that should never exist time to arrive before saying it did not.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        service.seen().is_empty(),
        "a gated handler still dispatched to an AI service: {:?}",
        service.seen()
    );
}

#[tokio::test]
async fn a_school_without_the_ai_package_dispatches_no_index_and_stores_no_output() {
    // The product rule: `chatbot` is the `ai` package, and a school that did
    // not buy it sends *nothing* to an AI service — course-note indexing
    // included, even though the notes themselves keep working. The note is
    // created (201), the service never hears about it, and no `rag_output`
    // row appears; selling `chatbot` back makes the very next note index.
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("indexer", &[AI_RAG_INDEX_CAPABILITY]),
        Behaviour::Answer(json!({ "summary": "x" })),
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;
    demo_without(&tenants, &[Module::Chatbot]).await;

    // `course_note` asserts the 201 itself: course notes stay fully usable.
    let (_cookie, note) = course_note(&app, &db).await;
    // The dispatch is fire-and-forget, so give a frame that should never exist
    // time to arrive before saying it did not.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        service.seen().is_empty(),
        "a school without the `ai` package still dispatched: {:?}",
        service.seen()
    );
    assert!(
        outputs(&db, &note).await.is_empty(),
        "no rag_output row may be written for a school without `chatbot`"
    );

    // Sold back: the next note indexes, so the module is the only thing that
    // was ever stopping it.
    demo_without(&tenants, &[]).await;
    let (_cookie, note) = course_note(&app, &db).await;
    let stored = await_outputs(&db, &note).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].get_payload()["summary"], "x");
    assert_eq!(service.seen()[0].capability, AI_RAG_INDEX_CAPABILITY);
}

#[tokio::test]
async fn a_blob_read_is_refused_module_disabled_when_the_school_has_no_chatbot() {
    // File bytes are the largest thing the backend would hand an AI service,
    // so the same `ai`-package rule holds on the blob stream: `course_notes`
    // on, `chatbot` off, and the stream still refuses.
    let bridge = bridge().await;
    let (service, student, file, uploaded, tenants) = blob_fixture(&bridge).await;

    demo_without(&tenants, &[Module::Chatbot]).await;
    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&student))).await;
    assert_eq!(blob_refusal(header), "module_disabled");
    assert!(bytes.is_empty(), "a refusal is followed by nothing at all");

    demo_without(&tenants, &[]).await;
    let (header, bytes) = blob_read(&service.conn, blob_of(&file, Some(&student))).await;
    assert!(
        matches!(header, BlobResponse::Ok { .. }),
        "the module is back: {}",
        blob_refusal(header)
    );
    assert!(bytes == uploaded, "the bytes differ from what was uploaded");
}

// ------------------------------------------------------------- telemetry --

/// The bridge's own observability, asserted end to end over real QUIC: one
/// dispatched request must produce an `ai.request` span naming the capability
/// and nothing about the caller, and one refused handshake must reach the
/// failure counter under a stable `reason`.
///
/// The only test in this binary that installs a subscriber — `tracing`'s
/// global default can be set once per process — so both assertions live in it
/// rather than in a test each. Everything is filtered by a capability no other
/// test uses, since the other tests keep dispatching into the same exporters.
#[tokio::test]
async fn a_dispatch_is_traced_by_capability_and_a_refused_handshake_is_counted() {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let span_exporter = InMemorySpanExporter::default();
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(span_exporter.clone())
        .build();
    let metric_exporter = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(metric_exporter.clone()).build())
        .build();
    // Installed before the bridge exists: its instruments are taken from
    // whatever meter provider is global when it first records.
    opentelemetry::global::set_meter_provider(meter_provider.clone());
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(tracer_provider.tracer("test")))
        .init();

    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("probe", &["telemetry.probe"]),
        Behaviour::Echo,
    )
    .await;
    await_workers(&bridge, 1).await;
    bridge
        .dispatch(&demo(), "telemetry.probe", json!({ "image": "abc" }))
        .await
        .expect("the service answered");

    // A refusal on its own connection, for the handshake counter.
    let mut wrong = hello("probe", &["telemetry.probe"]);
    wrong.token = "not-the-token".into();
    let (_endpoint, _conn, _send, _recv, greeting) = shake_hands(&bridge, wrong).await;
    assert!(
        matches!(greeting, Greeting::Rejected { .. }),
        "{greeting:?}"
    );

    tracer_provider.force_flush().expect("flush spans");
    let spans = span_exporter.get_finished_spans().expect("exported spans");
    let attrs_of = |span: &opentelemetry_sdk::trace::SpanData| -> Vec<(String, String)> {
        span.attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect()
    };
    let request = spans
        .iter()
        .find(|s| {
            s.name == "ai.request"
                && attrs_of(s)
                    .contains(&("ai.capability".to_string(), "telemetry.probe".to_string()))
        })
        .unwrap_or_else(|| {
            panic!(
                "no ai.request span named the probe capability: {:?}",
                spans.iter().map(|s| s.name.to_string()).collect::<Vec<_>>()
            )
        });
    let attrs = attrs_of(request);
    assert!(
        attrs.contains(&("school".to_string(), DEMO_SLUG.to_string())),
        "the span names the school slug: {attrs:?}"
    );
    assert!(
        attrs
            .iter()
            .any(|(k, v)| k == "ai.worker.id" && !v.is_empty()),
        "{attrs:?}"
    );
    assert!(
        attrs
            .iter()
            .any(|(k, v)| k == "ai.request.id" && !v.is_empty()),
        "{attrs:?}"
    );
    for span in &spans {
        for (key, _) in attrs_of(span) {
            assert!(
                !common::is_forbidden_key(&key),
                "span {:?} carries {key:?}, which may never leave this process",
                span.name
            );
        }
    }

    meter_provider.force_flush().expect("flush metrics");
    let mut refused = 0u64;
    for resource in metric_exporter.get_finished_metrics().expect("metrics") {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics() {
                if metric.name() != "ai_handshake_failures_total" {
                    continue;
                }
                if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
                    for point in sum.data_points() {
                        if point.attributes().any(|kv| {
                            kv.key.as_str() == "reason" && kv.value.to_string() == "bad_token"
                        }) {
                            refused += point.value();
                        }
                    }
                }
            }
        }
    }
    assert!(
        refused >= 1,
        "the refused handshake must be counted under reason=bad_token"
    );
}
