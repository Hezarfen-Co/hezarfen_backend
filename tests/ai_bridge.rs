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
                    Behaviour::Reply(text) => Some(Response::Ok {
                        id: request.id.clone(),
                        payload: json!({ "text": text }),
                    }),
                    Behaviour::EchoPrompt => {
                        let asked = request.payload["message"]
                            .as_str()
                            .unwrap_or("<no message>");
                        Some(Response::Ok {
                            id: request.id.clone(),
                            payload: json!({ "text": format!("cevap::{asked}") }),
                        })
                    }
                    Behaviour::Fail { code, message } => Some(Response::Err {
                        id: request.id.clone(),
                        code,
                        message,
                    }),
                    Behaviour::WrongId => Some(Response::Ok {
                        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string(),
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
        .dispatch("ocr.extract", json!({ "image": "abc" }))
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
        tokio::spawn(async move { bridge.dispatch("ocr.extract", json!({ "n": i })).await })
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

    bridge.dispatch("grade.essay", json!("text")).await.unwrap();
    bridge.dispatch("ocr.extract", json!("png")).await.unwrap();

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
        bridge.dispatch("ocr.extract", json!(null)).await,
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
        .dispatch("grade.essay", json!(null))
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
        .dispatch("ocr.extract", json!({ "image": "x" }))
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
        .dispatch("ocr.extract", json!(null))
        .await
        .unwrap_err();
    assert!(matches!(err, AiError::Timeout(300)), "{err}");
    // The request did reach the service — this is a timeout, not a delivery
    // failure, which is exactly why the error cannot promise it did not run.
    assert_eq!(service.seen().len(), 1);
    // An abandoned request must not leak the worker's capacity.
    assert_eq!(bridge.workers()[0].inflight, 0);
    // And the worker is immediately usable again.
    assert!(bridge.dispatch("ocr.extract", json!(null)).await.is_err());
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
        bridge.dispatch("ocr.extract", json!(null)).await,
        Err(AiError::Timeout(100))
    ));
    // ...but a capability known to be slow can ask for more.
    let answer = bridge
        .dispatch_with_timeout("ocr.extract", json!("slow"), Duration::from_secs(5))
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
        tokio::spawn(async move { bridge.dispatch("ocr.extract", json!(null)).await })
    };
    await_inflight(&bridge, 1).await;

    let err = bridge
        .dispatch("ocr.extract", json!(null))
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
        .dispatch("ocr.extract", json!(null))
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
        .dispatch("ocr.extract", json!(null))
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
    bridge.dispatch("ocr.extract", json!(null)).await.unwrap();

    service.conn.close(0u32.into(), b"service shutting down");
    drop(service);
    await_workers(&bridge, 0).await;

    assert!(!bridge.has_capability("ocr.extract"));
    let err = bridge
        .dispatch("ocr.extract", json!(null))
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
        .dispatch("ocr.extract", json!("again"))
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
        bridge.dispatch("ocr.extract", json!("x")).await.unwrap();
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
        .dispatch("ocr.extract", json!({ "image": blob }))
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
    let db = hezarfen_backend::database::init_mem()
        .await
        .expect("mem db");
    let app = hezarfen_backend::build_router(hezarfen_backend::state::AppState {
        db,
        files_path: std::env::temp_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai,
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
use surrealdb::types::RecordId;

/// A router wired to `bridge`, plus a handle to its in-memory database.
async fn chat_app(bridge: &AiBridge) -> (Router, Database) {
    let db = hezarfen_backend::database::init_mem()
        .await
        .expect("mem db");
    let app = hezarfen_backend::build_router(hezarfen_backend::state::AppState {
        db: db.clone(),
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: Some(bridge.clone()),
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

    db.query(
        "CREATE chatbot_message:h3 SET thread_id = $conv, user_id = $usr, role = 'assistant',
             content = '', status = 'pending', created_at = 1002;",
    )
    .bind(("conv", RecordId::new("chatbot_thread", thread.as_str())))
    .bind(("usr", RecordId::new("user", user.as_str())))
    .await
    .expect("seed history")
    .check()
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
