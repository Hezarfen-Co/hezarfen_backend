//! Wire-contract tests for the AI bridge — the regression net for `hab/1`.
//!
//! # Why this file exists separately from `ai_bridge.rs`
//!
//! `ai_bridge.rs` drives real QUIC clients, but its fake service calls the
//! library's own `write_frame` / `read_frame` and its own `Hello` / `Request`
//! structs. Both ends therefore share one Rust definition, so it cannot notice
//! a change to the wire: rename `deadline_ms` to `timeout_ms` and every test
//! there still passes, while every service written in another language breaks.
//!
//! So nothing here imports `hezarfen_backend::ai::protocol`. Frames are built
//! as byte literals and parsed as untyped JSON, exactly as a Python or Go
//! service would. A field rename, a re-tagged enum, a flipped length-prefix
//! endianness, or a newly-required field fails a test here instead of failing
//! in a service repo.
//!
//! It doubles as the reference implementation: `raw::Service` below is the
//! whole client side of `hab/1` in about a hundred lines.

use std::sync::Arc;
use std::time::Duration;

use hezarfen_backend::ai::{AiBridge, AiError, BridgeConfig};
use serde_json::{Value, json};

const TOKEN: &str = "shared-ai-token";

/// The protocol identifier this suite pins. Deliberately a literal, not
/// `AI_PROTOCOL`: importing the constant would let a rename sail through, and
/// the whole point is that the string is a published contract.
const PROTOCOL: &str = "hab/1";

// ------------------------------------------------------------------- raw --

/// A minimal `hab/1` client with no dependency on the crate's protocol types.
mod raw {
    use super::*;

    /// Length-prefix one JSON body: `u32` **big-endian** byte count, then the
    /// bytes. Written out by hand so an endianness change is caught here.
    pub fn frame(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + body.len());
        let len = body.len() as u32;
        out.push((len >> 24) as u8);
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push(len as u8);
        out.extend_from_slice(body);
        out
    }

    /// Read exactly `n` bytes, or fail.
    async fn read_exact(recv: &mut quinn::RecvStream, n: usize) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; n];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| format!("short read of {n} bytes: {e}"))?;
        Ok(buf)
    }

    /// Read one frame and parse it as untyped JSON, plus the raw bytes so a
    /// test can assert on the encoding itself.
    pub async fn read_frame(recv: &mut quinn::RecvStream) -> Result<(Value, Vec<u8>), String> {
        let header = read_exact(recv, 4).await?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let body = read_exact(recv, len).await?;
        let value =
            serde_json::from_slice(&body).map_err(|e| format!("frame was not JSON: {e}"))?;
        Ok((value, body))
    }

    /// A QUIC endpoint trusting exactly the bridge's certificate — what a
    /// service does after `GET /ai/certificate`.
    pub fn endpoint(bridge: &AiBridge) -> quinn::Endpoint {
        hezarfen_backend::ai::tls::install_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(bridge.certificate()).expect("pin the leaf");
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![PROTOCOL.as_bytes().to_vec()];
        finish_endpoint(tls)
    }

    /// Same, but announcing some other ALPN — for the version-gate test.
    pub fn endpoint_with_alpn(bridge: &AiBridge, alpn: &[u8]) -> quinn::Endpoint {
        hezarfen_backend::ai::tls::install_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(bridge.certificate()).expect("pin the leaf");
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![alpn.to_vec()];
        finish_endpoint(tls)
    }

    fn finish_endpoint(tls: rustls::ClientConfig) -> quinn::Endpoint {
        let mut config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC-usable TLS"),
        ));
        let mut transport = quinn::TransportConfig::default();
        // Requests arrive as server-initiated streams; this is the ceiling on
        // how many the bridge may open toward us.
        transport.max_concurrent_bidi_streams(128u32.into());
        config.transport_config(Arc::new(transport));
        let mut endpoint =
            quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
        endpoint.set_default_client_config(config);
        endpoint
    }

    /// A connected raw service: the QUIC connection plus its control stream.
    pub struct Service {
        pub _endpoint: quinn::Endpoint,
        pub conn: quinn::Connection,
        pub control: (quinn::SendStream, quinn::RecvStream),
        /// The bridge's answer to the `Hello`, as untyped JSON.
        pub greeting: Value,
    }

    /// Dial and send `hello_body` verbatim on the control stream.
    pub async fn handshake(bridge: &AiBridge, hello_body: &[u8]) -> Service {
        let endpoint = endpoint(bridge);
        let conn = endpoint
            .connect(bridge.local_addr().unwrap(), "localhost")
            .expect("dial")
            .await
            .expect("QUIC handshake");
        let (mut send, mut recv) = conn.open_bi().await.expect("control stream");
        send.write_all(&frame(hello_body))
            .await
            .expect("write Hello");
        let (greeting, _) = read_frame(&mut recv).await.expect("read Greeting");
        Service {
            _endpoint: endpoint,
            conn,
            control: (send, recv),
            greeting,
        }
    }

    /// A well-formed `Hello` for a service offering one capability.
    pub fn hello(service: &str, capability: &str) -> Vec<u8> {
        format!(
            r#"{{"protocol":"{PROTOCOL}","service":"{service}","capabilities":["{capability}"],"token":"{TOKEN}"}}"#
        )
        .into_bytes()
    }

    /// Accept one request stream, returning the parsed request, its raw bytes,
    /// and the stream to answer on.
    pub async fn take_request(
        conn: &quinn::Connection,
    ) -> (Value, Vec<u8>, quinn::SendStream, quinn::RecvStream) {
        let (send, mut recv) = conn
            .accept_bi()
            .await
            .expect("bridge opened a request stream");
        let (request, bytes) = read_frame(&mut recv).await.expect("read Request");
        (request, bytes, send, recv)
    }

    /// Answer a request stream with `body` verbatim, then finish.
    pub async fn answer(mut send: quinn::SendStream, body: &[u8]) {
        send.write_all(&frame(body)).await.expect("write Response");
        let _ = send.finish();
        let _ = send.stopped().await;
    }

    /// The set of top-level keys of a JSON object, sorted.
    pub fn keys(value: &Value) -> Vec<String> {
        let mut k: Vec<String> = value
            .as_object()
            .expect("a JSON object")
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    }
}

// --------------------------------------------------------------- harness --

async fn bridge_with_timeout(timeout: Duration) -> AiBridge {
    AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: timeout,
    })
    .await
    .expect("bridge binds")
}

async fn bridge() -> AiBridge {
    bridge_with_timeout(Duration::from_secs(5)).await
}

async fn await_workers(bridge: &AiBridge, expected: usize) {
    for _ in 0..300 {
        if bridge.workers().len() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {expected} workers, saw {:?}", bridge.workers());
}

/// Dispatch on a background task so the test can play the service in the
/// foreground.
fn dispatch(
    bridge: &AiBridge,
    capability: &str,
    payload: Value,
) -> tokio::task::JoinHandle<Result<Value, AiError>> {
    let bridge = bridge.clone();
    let capability = capability.to_string();
    tokio::spawn(async move { bridge.dispatch(&capability, payload).await })
}

// ------------------------------------------------------- framing contract --

#[tokio::test]
async fn the_length_prefix_is_four_byte_big_endian() {
    // Flipping to little-endian would still round-trip between two Rust peers
    // that share this crate — and break every foreign service instantly. Here
    // the header is read as four separate bytes and reassembled by hand.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let call = dispatch(&bridge, "ocr.extract", json!({ "k": "v" }));
    let (_send, mut recv) = service.conn.accept_bi().await.unwrap();

    let mut header = [0u8; 4];
    recv.read_exact(&mut header)
        .await
        .expect("four header bytes");
    let big_endian = u32::from_be_bytes(header) as usize;
    let little_endian = u32::from_le_bytes(header) as usize;

    let mut body = vec![0u8; big_endian];
    recv.read_exact(&mut body)
        .await
        .expect("the body the header promised");
    assert_eq!(body.len(), big_endian);
    // A real request frame is well under 16 MiB, so the two top bytes are zero
    // and only the big-endian reading is plausible.
    assert_eq!(
        header[0], 0,
        "top byte of a small frame's length must be zero"
    );
    assert_eq!(header[1], 0);
    assert_ne!(
        big_endian, little_endian,
        "test is only meaningful when the two readings differ"
    );
    assert!(
        serde_json::from_slice::<Value>(&body).is_ok(),
        "the big-endian length delimited exactly one JSON document"
    );
    call.abort();
}

#[tokio::test]
async fn a_frame_split_across_many_writes_is_reassembled() {
    // QUIC delivers a stream, not messages. A reader that assumed one read
    // yields one whole frame would pass every other test and fail under real
    // network conditions, so the Hello is dribbled out a byte at a time.
    let bridge = bridge().await;
    let endpoint = raw::endpoint(&bridge);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();

    let bytes = raw::frame(&raw::hello("ocr", "ocr.extract"));
    for byte in &bytes {
        send.write_all(&[*byte]).await.expect("dribble");
        tokio::task::yield_now().await;
    }

    let (greeting, _) = raw::read_frame(&mut recv).await.expect("still welcomed");
    assert_eq!(greeting["type"], "welcome");
    await_workers(&bridge, 1).await;
}

#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused_without_the_body() {
    // The guard is the reason a bad four-byte header cannot make the backend
    // reserve gigabytes. Only the header is ever sent; the bridge must reject
    // on that alone rather than waiting for a body that never comes.
    let bridge = bridge().await;
    let endpoint = raw::endpoint(&bridge);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .unwrap()
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let oversized: u32 = 9 * 1024 * 1024; // over the 8 MiB cap
    send.write_all(&oversized.to_be_bytes()).await.unwrap();

    // No body follows. The bridge must not register the service, and must not
    // still be waiting when the handshake timeout would otherwise apply.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        bridge.workers().is_empty(),
        "an oversized frame must not produce a registration"
    );
}

#[tokio::test]
async fn a_zero_length_frame_is_rejected_not_treated_as_empty() {
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, b"").await;
    assert_eq!(service.greeting["type"], "rejected");
    assert_eq!(service.greeting["code"], "malformed");
    assert!(bridge.workers().is_empty());
}

// ------------------------------------------------- handshake key contract --

#[tokio::test]
async fn the_welcome_frame_carries_exactly_the_published_keys() {
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;

    assert_eq!(
        raw::keys(&service.greeting),
        ["protocol", "type", "worker_id"]
    );
    assert_eq!(service.greeting["type"], "welcome");
    assert_eq!(service.greeting["protocol"], PROTOCOL);
    assert!(
        service.greeting["worker_id"]
            .as_str()
            .is_some_and(|w| !w.is_empty()),
        "worker_id is a non-empty string a service can log"
    );
}

#[tokio::test]
async fn the_rejection_frame_carries_exactly_the_published_keys_and_codes() {
    // A service switches on these literals. Re-tagging the enum or renaming a
    // variant is a breaking change and must fail here.
    let bridge = bridge().await;

    let cases: [(&str, Vec<u8>); 4] = [
        (
            "unauthorized",
            format!(
                r#"{{"protocol":"{PROTOCOL}","service":"s","capabilities":["c"],"token":"wrong"}}"#
            )
            .into_bytes(),
        ),
        (
            "unsupported_protocol",
            format!(
                r#"{{"protocol":"hab/99","service":"s","capabilities":["c"],"token":"{TOKEN}"}}"#
            )
            .into_bytes(),
        ),
        (
            "no_capabilities",
            format!(
                r#"{{"protocol":"{PROTOCOL}","service":"s","capabilities":[],"token":"{TOKEN}"}}"#
            )
            .into_bytes(),
        ),
        ("malformed", b"not json".to_vec()),
    ];

    for (expected_code, hello) in cases {
        let service = raw::handshake(&bridge, &hello).await;
        assert_eq!(
            raw::keys(&service.greeting),
            ["code", "message", "type"],
            "rejection shape changed for {expected_code}"
        );
        assert_eq!(service.greeting["type"], "rejected");
        assert_eq!(
            service.greeting["code"], expected_code,
            "reject code spelling changed: {}",
            service.greeting
        );
        assert!(
            service.greeting["message"].as_str().is_some(),
            "message must be a string a service can log"
        );
    }
    assert!(bridge.workers().is_empty());
}

#[tokio::test]
async fn hello_accepts_unknown_fields_so_services_can_run_ahead() {
    // Forward compatibility: a newer service sending a field this backend does
    // not know must still register, or every rollout becomes lockstep.
    let bridge = bridge().await;
    let hello = format!(
        r#"{{"protocol":"{PROTOCOL}","service":"ocr","capabilities":["ocr.extract"],"token":"{TOKEN}","future_field":{{"nested":true}},"model":"gpt-9"}}"#
    );
    let service = raw::handshake(&bridge, hello.as_bytes()).await;
    assert_eq!(service.greeting["type"], "welcome");
    await_workers(&bridge, 1).await;
}

#[tokio::test]
async fn hello_requires_its_documented_fields() {
    // The mirror of the test above: dropping a *required* field must be a
    // clean `malformed` rejection, never a silent default that registers a
    // half-configured worker.
    let bridge = bridge().await;
    let required = ["protocol", "service", "capabilities", "token"];

    for missing in required {
        let mut fields = json!({
            "protocol": PROTOCOL,
            "service": "ocr",
            "capabilities": ["ocr.extract"],
            "token": TOKEN,
        });
        fields.as_object_mut().unwrap().remove(missing);
        let body = serde_json::to_vec(&fields).unwrap();

        let service = raw::handshake(&bridge, &body).await;
        assert_eq!(
            service.greeting["type"], "rejected",
            "a Hello without `{missing}` was accepted"
        );
        assert_eq!(service.greeting["code"], "malformed");
    }
    assert!(bridge.workers().is_empty());
}

#[tokio::test]
async fn max_concurrent_stays_optional() {
    // `raw::hello` omits it entirely — a service that never sets it must still
    // register and get the default.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    assert_eq!(service.greeting["type"], "welcome");
    await_workers(&bridge, 1).await;
    assert!(
        bridge.workers()[0].max_concurrent >= 1,
        "an omitted max_concurrent yields a usable default"
    );
}

#[tokio::test]
async fn a_foreign_alpn_is_refused_at_the_tls_handshake() {
    // The version gate: an incompatible service must be stopped before it can
    // send a frame this backend would misparse.
    let bridge = bridge().await;
    let endpoint = raw::endpoint_with_alpn(&bridge, b"hab/99");
    let result = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .unwrap()
        .await;
    assert!(result.is_err(), "ALPN hab/99 must not connect");
    assert!(bridge.workers().is_empty());
}

// --------------------------------------------------- request key contract --

#[tokio::test]
async fn the_request_frame_carries_exactly_the_published_keys() {
    // The contract a foreign service parses. An added, removed or renamed key
    // fails here — this is the assertion `ai_bridge.rs` structurally cannot
    // make, because there both ends share one Rust struct.
    let bridge = bridge_with_timeout(Duration::from_secs(7)).await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let call = dispatch(&bridge, "ocr.extract", json!({ "image": "abc", "n": 3 }));
    let (request, _bytes, send, _recv) = raw::take_request(&service.conn).await;

    assert_eq!(
        raw::keys(&request),
        ["capability", "deadline_ms", "id", "payload"],
        "request shape changed: {request}"
    );
    assert_eq!(request["capability"], "ocr.extract");
    assert_eq!(request["deadline_ms"], 7000);
    assert!(request["id"].as_str().is_some_and(|i| !i.is_empty()));
    // The payload is passed through untouched — the transport must not wrap,
    // stringify or reorder a caller's body.
    assert_eq!(request["payload"], json!({ "image": "abc", "n": 3 }));

    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"ok","id":"{id}","payload":{{"text":"hi"}}}}"#).as_bytes(),
    )
    .await;
    let answer = call.await.unwrap().expect("dispatch succeeded");
    assert_eq!(answer, json!({ "text": "hi" }));
}

#[tokio::test]
async fn a_payload_of_any_json_shape_survives_unchanged() {
    // The transport is payload-agnostic by contract. Anything that "helpfully"
    // normalised a body — dropping nulls, coercing numbers, flattening — would
    // corrupt a capability's data in a way no typed test would notice.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let payloads = [
        json!(null),
        json!(true),
        json!(-17),
        json!(1.5),
        json!("düz metin — ünicode"),
        json!([]),
        json!([1, [2, [3]], { "deep": { "deeper": null } }]),
        json!({ "empty": {}, "zero": 0, "false": false, "nul": null }),
    ];

    for payload in payloads {
        let call = dispatch(&bridge, "ocr.extract", payload.clone());
        let (request, _, send, _recv) = raw::take_request(&service.conn).await;
        assert_eq!(request["payload"], payload, "payload mutated in flight");

        let id = request["id"].as_str().unwrap();
        let echo = json!({ "status": "ok", "id": id, "payload": payload });
        raw::answer(send, &serde_json::to_vec(&echo).unwrap()).await;
        assert_eq!(
            call.await.unwrap().unwrap(),
            payload,
            "payload mutated on return"
        );
    }
}

#[tokio::test]
async fn the_response_tags_are_the_published_literals() {
    // A service *writes* these, so the backend's parser is the contract. Both
    // spellings are exercised, plus a wrong one to prove they are checked.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let ok = dispatch(&bridge, "ocr.extract", json!(1));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"ok","id":"{id}","payload":42}}"#).as_bytes(),
    )
    .await;
    assert_eq!(ok.await.unwrap().unwrap(), json!(42));

    let err = dispatch(&bridge, "ocr.extract", json!(2));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"err","id":"{id}","code":"bad_input","message":"nope"}}"#).as_bytes(),
    )
    .await;
    match err.await.unwrap() {
        Err(AiError::Remote { code, message }) => {
            assert_eq!(code, "bad_input");
            assert_eq!(message, "nope");
        }
        other => panic!("expected a remote error, got {other:?}"),
    }

    let bogus = dispatch(&bridge, "ocr.extract", json!(3));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"success","id":"{id}","payload":1}}"#).as_bytes(),
    )
    .await;
    assert!(
        matches!(bogus.await.unwrap(), Err(AiError::Protocol(_))),
        "an unpublished status tag must be refused, not guessed at"
    );
}

#[tokio::test]
async fn a_response_may_carry_unknown_fields() {
    // Same forward-compatibility rule as `Hello`: a newer service adding a
    // field (timings, model name) must not break an older backend.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let call = dispatch(&bridge, "ocr.extract", json!(null));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(
            r#"{{"status":"ok","id":"{id}","payload":{{"text":"x"}},"took_ms":91,"model":"v2"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(call.await.unwrap().unwrap(), json!({ "text": "x" }));
}

#[tokio::test]
async fn a_response_missing_its_id_is_refused() {
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let call = dispatch(&bridge, "ocr.extract", json!(null));
    let (_request, _, send, _r) = raw::take_request(&service.conn).await;
    raw::answer(send, br#"{"status":"ok","payload":1}"#).await;
    assert!(
        matches!(call.await.unwrap(), Err(AiError::Protocol(_))),
        "id is required on the wire"
    );
}

// ------------------------------------------------------- stream behaviour --

#[tokio::test]
async fn each_request_gets_its_own_stream_and_answers_may_come_back_in_any_order() {
    // The core promise of using QUIC: correlation is the stream, so answering
    // out of order — the natural case when inference times differ — must route
    // every payload to its own caller.
    let bridge = bridge_with_timeout(Duration::from_secs(20)).await;
    let hello = format!(
        r#"{{"protocol":"{PROTOCOL}","service":"ocr","capabilities":["ocr.extract"],"token":"{TOKEN}","max_concurrent":8}}"#
    );
    let service = raw::handshake(&bridge, hello.as_bytes()).await;
    await_workers(&bridge, 1).await;

    let calls: Vec<_> = (0..5)
        .map(|i| dispatch(&bridge, "ocr.extract", json!({ "n": i })))
        .collect();

    // Collect all five streams before answering any: they must all be open at
    // once, which is only true if each request took a fresh stream.
    let mut pending = Vec::new();
    for _ in 0..5 {
        let (request, _, send, _recv) = raw::take_request(&service.conn).await;
        pending.push((request, send));
    }

    // Answer in reverse arrival order.
    pending.reverse();
    for (request, send) in pending {
        let id = request["id"].as_str().unwrap().to_string();
        let n = request["payload"]["n"].clone();
        raw::answer(
            send,
            format!(r#"{{"status":"ok","id":"{id}","payload":{{"n":{n}}}}}"#).as_bytes(),
        )
        .await;
    }

    for (i, call) in calls.into_iter().enumerate() {
        let answer = call.await.unwrap().expect("dispatch ok");
        assert_eq!(
            answer["n"], i as u64,
            "answer {i} came back on the wrong caller's future"
        );
    }
}

#[tokio::test]
async fn request_ids_are_unique_per_request() {
    // The id is only a trace id, but a repeated one makes production logs
    // unreadable and would silently pass the id-echo check.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let mut seen = std::collections::HashSet::new();
    for _ in 0..6 {
        let call = dispatch(&bridge, "ocr.extract", json!(null));
        let (request, _, send, _r) = raw::take_request(&service.conn).await;
        let id = request["id"].as_str().unwrap().to_string();
        assert!(seen.insert(id.clone()), "id {id} was reused");
        raw::answer(
            send,
            format!(r#"{{"status":"ok","id":"{id}","payload":1}}"#).as_bytes(),
        )
        .await;
        call.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn the_control_stream_carries_no_frames_after_the_welcome() {
    // Its only remaining job is to be open. If the backend ever started
    // expecting heartbeats here, a service that just holds it open would be
    // dropped — so assert the silence is fine, then prove the worker still
    // serves.
    let bridge = bridge().await;
    let mut service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let quiet = tokio::time::timeout(
        Duration::from_millis(400),
        raw::read_frame(&mut service.control.1),
    )
    .await;
    assert!(
        quiet.is_err(),
        "nothing should arrive on the control stream"
    );

    let call = dispatch(&bridge, "ocr.extract", json!(null));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"ok","id":"{id}","payload":1}}"#).as_bytes(),
    )
    .await;
    call.await
        .unwrap()
        .expect("still serving after the silence");
}

#[tokio::test]
async fn closing_the_control_stream_deregisters_the_service() {
    // The documented goodbye. A service that finishes the control stream is
    // saying "I am going away" even though the QUIC connection is still up.
    let bridge = bridge().await;
    let mut service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let _ = service.control.0.finish();
    drop(service.control);
    service.conn.close(0u32.into(), b"goodbye");
    await_workers(&bridge, 0).await;

    assert!(matches!(
        bridge.dispatch("ocr.extract", json!(null)).await,
        Err(AiError::NoWorker(_))
    ));
}

#[tokio::test]
async fn a_service_that_drops_a_request_stream_fails_that_request_only() {
    // A crash mid-inference. The caller must get a transport error, and the
    // connection must remain usable for the next request rather than being
    // torn down.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let doomed = dispatch(&bridge, "ocr.extract", json!("first"));
    let (_request, _, send, recv) = raw::take_request(&service.conn).await;
    drop(send);
    drop(recv);
    let err = doomed
        .await
        .unwrap()
        .expect_err("the dropped stream failed");
    assert!(
        matches!(err, AiError::Transport(_) | AiError::Protocol(_)),
        "unexpected error: {err}"
    );

    assert_eq!(
        bridge.workers().len(),
        1,
        "the worker survives one bad stream"
    );
    let next = dispatch(&bridge, "ocr.extract", json!("second"));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"ok","id":"{id}","payload":"ok"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(next.await.unwrap().unwrap(), json!("ok"));
}

#[tokio::test]
async fn a_late_answer_after_the_deadline_is_not_delivered() {
    // The deadline is a promise to the caller, not a hint. A service that
    // ignores `deadline_ms` must not be able to resolve a future that already
    // failed.
    let bridge = bridge_with_timeout(Duration::from_millis(250)).await;
    let service = raw::handshake(&bridge, &raw::hello("ocr", "ocr.extract")).await;
    await_workers(&bridge, 1).await;

    let call = dispatch(&bridge, "ocr.extract", json!(null));
    let (request, _, mut send, _r) = raw::take_request(&service.conn).await;
    assert_eq!(
        request["deadline_ms"], 250,
        "the service was told the deadline"
    );

    assert!(
        matches!(call.await.unwrap(), Err(AiError::Timeout(250))),
        "the call fails at the deadline"
    );
    assert_eq!(bridge.workers()[0].inflight, 0, "the slot was released");

    // Abandoning the request also tears the stream down, so a service that
    // ignored `deadline_ms` finds out: the backend stops the stream and the
    // service's send half reports it. Not synchronous — a QUIC reset costs a
    // round trip — so wait for it rather than probing once.
    //
    // This is why a service should read a write failure on a request stream as
    // "the caller gave up", not as a bug, and stop the work.
    let stopped = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    assert!(
        stopped.is_ok(),
        "the abandoned stream must be torn down, not left open for a late answer"
    );

    // And the caller is unaffected by whatever the service does afterwards:
    // the worker is idle and immediately reusable.
    assert_eq!(bridge.workers()[0].inflight, 0);
}

#[tokio::test]
async fn several_raw_services_serve_one_capability_together() {
    // Scale-out as a service author experiences it: three independent
    // connections, no backend config change, every request answered exactly
    // once by exactly one of them.
    let bridge = bridge_with_timeout(Duration::from_secs(20)).await;
    let mut services = Vec::new();
    for i in 0..3 {
        services
            .push(raw::handshake(&bridge, &raw::hello(&format!("ocr-{i}"), "ocr.extract")).await);
    }
    await_workers(&bridge, 3).await;

    // Each service answers whatever it is handed, forever.
    for service in &services {
        let conn = service.conn.clone();
        tokio::spawn(async move {
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let Ok((request, _)) = raw::read_frame(&mut recv).await else {
                    return;
                };
                let id = request["id"].as_str().unwrap().to_string();
                let n = request["payload"]["n"].clone();
                raw::answer(
                    send,
                    format!(r#"{{"status":"ok","id":"{id}","payload":{n}}}"#).as_bytes(),
                )
                .await;
            }
        });
    }

    let calls: Vec<_> = (0..12)
        .map(|n| dispatch(&bridge, "ocr.extract", json!({ "n": n })))
        .collect();
    let mut got: Vec<u64> = Vec::new();
    for call in calls {
        got.push(call.await.unwrap().unwrap().as_u64().expect("echoed n"));
    }
    got.sort_unstable();
    assert_eq!(
        got,
        (0..12).collect::<Vec<u64>>(),
        "every request answered once"
    );
}
