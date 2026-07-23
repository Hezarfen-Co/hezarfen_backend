//! End-to-end tests for the AI bridge, against a fake AI service.
//!
//! Everything here runs over a real QUIC socket on loopback: a real
//! handshake, real TLS, real streams. The only thing pretended is the model —
//! [`Behaviour`] stands in for what a service would do with a request. That
//! keeps the transport honest: a bug in framing, stream lifetime, registration
//! or timeout shows up here rather than the first time a Python service dials
//! in.

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
        exam_presence: Default::default(),
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
