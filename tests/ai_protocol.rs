//! Wire-contract tests for the AI bridge — the regression net for `hab/2`.
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
//! whole client side of `hab/2` in about a hundred lines.

mod common;

use std::sync::Arc;
use std::time::Duration;

use hezarfen_backend::ai::{AiBridge, AiError, BridgeConfig};
use hezarfen_backend::module::ModuleSet;
use hezarfen_backend::tenant::{SchoolStatus, Slug, Tenants};
use serde_json::{Value, json};

const TOKEN: &str = "shared-ai-token";

/// The school every frame in this suite names. A literal for the same reason
/// `PROTOCOL` is one: a service spells the slug out, it is not a Rust constant.
const SCHOOL: &str = "demo";

/// The protocol identifier this suite pins. Deliberately a literal, not
/// `AI_PROTOCOL`: importing the constant would let a rename sail through, and
/// the whole point is that the string is a published contract.
const PROTOCOL: &str = "hab/2";

// ------------------------------------------------------------------- raw --

/// A minimal `hab/2` client with no dependency on the crate's protocol types.
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

    /// Send one api-read frame verbatim on a fresh *client*-initiated stream
    /// and read the single answer frame. This is the whole api-read client:
    /// after the handshake a service opens a stream, writes an `ApiRequest`,
    /// and reads one `ApiResponse`.
    pub async fn api_read(conn: &quinn::Connection, body: &[u8]) -> (Value, Vec<u8>) {
        let (mut send, mut recv) = conn.open_bi().await.expect("api-read stream");
        send.write_all(&frame(body))
            .await
            .expect("write ApiRequest");
        let _ = send.finish();
        read_frame(&mut recv).await.expect("read ApiResponse")
    }

    /// Send one frame verbatim on a fresh *client*-initiated stream, read the
    /// single answer frame, and then read whatever raw bytes followed it. That
    /// is the whole blob client: one `BlobRequest`, one header frame, then
    /// exactly `size` bytes and EOF.
    pub async fn blob_read(conn: &quinn::Connection, body: &[u8]) -> (Value, Vec<u8>, Vec<u8>) {
        let (mut send, mut recv) = conn.open_bi().await.expect("blob stream");
        send.write_all(&frame(body))
            .await
            .expect("write BlobRequest");
        let _ = send.finish();
        let (header, bytes) = read_frame(&mut recv).await.expect("read BlobResponse");
        // Deliberately asks for more than the header promised: a stream that
        // wrote one byte too many would show up here, not as a silent pass.
        let body = recv
            .read_to_end(16 * 1024 * 1024)
            .await
            .expect("read to EOF after the header");
        (header, bytes, body)
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
    let school = hezarfen_backend::tenant::Slug::try_new(SCHOOL).expect("the demo slug");
    tokio::spawn(async move { bridge.dispatch(&school, &capability, payload).await })
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

    let cases: [(&str, Vec<u8>); 5] = [
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
        // The immediate predecessor, named on purpose: `hab/1` frames carry no
        // school, so a service still speaking it must be turned away here
        // rather than reaching a path that would have to guess one.
        (
            "unsupported_protocol",
            format!(
                r#"{{"protocol":"hab/1","service":"s","capabilities":["c"],"token":"{TOKEN}"}}"#
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
    // `hab/1` included by name: the version gate is the *first* line of defence
    // against a service whose frames name no school, ahead of the Hello check.
    for alpn in [b"hab/99".as_slice(), b"hab/1".as_slice()] {
        let endpoint = raw::endpoint_with_alpn(&bridge, alpn);
        let result = endpoint
            .connect(bridge.local_addr().unwrap(), "localhost")
            .unwrap()
            .await;
        assert!(
            result.is_err(),
            "ALPN {} must not connect",
            String::from_utf8_lossy(alpn)
        );
    }
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
        ["capability", "deadline_ms", "id", "payload", "school"],
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
        format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":{{"text":"hi"}}}}"#)
            .as_bytes(),
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
        let echo = json!({ "status": "ok", "id": id, "school": SCHOOL, "payload": payload });
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
        format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":42}}"#).as_bytes(),
    )
    .await;
    assert_eq!(ok.await.unwrap().unwrap(), json!(42));

    let err = dispatch(&bridge, "ocr.extract", json!(2));
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
    let id = request["id"].as_str().unwrap().to_string();
    raw::answer(
        send,
        format!(r#"{{"status":"err","id":"{id}","school":"{SCHOOL}","code":"bad_input","message":"nope"}}"#).as_bytes(),
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
        format!(r#"{{"status":"success","id":"{id}","school":"{SCHOOL}","payload":1}}"#).as_bytes(),
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
            r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":{{"text":"x"}},"took_ms":91,"model":"v2"}}"#
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
            format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":{{"n":{n}}}}}"#)
                .as_bytes(),
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
            format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":1}}"#).as_bytes(),
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
        format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":1}}"#).as_bytes(),
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
        bridge
            .dispatch(
                &hezarfen_backend::tenant::Slug::try_new(SCHOOL).unwrap(),
                "ocr.extract",
                json!(null)
            )
            .await,
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
        format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":"ok"}}"#).as_bytes(),
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
    let (request, _, send, _r) = raw::take_request(&service.conn).await;
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
                    format!(r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":{n}}}"#)
                        .as_bytes(),
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

// ------------------------------------------------------ capability payload --
//
// The frame shapes above are capability-agnostic. `chat.reply` is the one
// capability whose *payload* is a published contract too, and the payload is
// built by the HTTP handler — so the only way to pin it as a foreign service
// sees it is to drive the real endpoint and read the bytes off the wire.

#[tokio::test]
async fn a_chat_request_names_the_askers_school_role() {
    // A chat service scopes its answer by who is asking, so the payload must
    // carry the asker's school role as a bare lowercase string — read here out
    // of the raw frame bytes, never through the crate's own payload struct.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;

    let tenants = hezarfen_backend::database::init_mem_tenants()
        .await
        .expect("in-memory deployment");
    let db = tenants
        .get(&hezarfen_backend::tenant::Slug::try_new(hezarfen_backend::tenant::DEMO_SLUG).unwrap())
        .await
        .expect("the demo school");
    let app = hezarfen_backend::build_router(hezarfen_backend::state::AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: Some(bridge.clone()),
    });
    let cookie = common::login_as(&app, &db, "veli", "teacher").await;
    let res = common::send(
        &app,
        "POST",
        "/chatbot/threads",
        Some(&cookie),
        Some(json!({})),
    )
    .await;
    let thread = common::id_of(&res.body);
    let res = common::send(
        &app,
        "POST",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(&cookie),
        Some(json!({ "content": "ikinci yasa nedir?" })),
    )
    .await;
    assert_eq!(res.status, axum::http::StatusCode::ACCEPTED, "{}", res.body);

    let (request, bytes, send, _recv) = raw::take_request(&service.conn).await;
    assert_eq!(request["capability"], "chat.reply");
    assert_eq!(
        raw::keys(&request["payload"]),
        ["asker_role", "history", "message"],
        "the documented chat request payload keys"
    );
    assert_eq!(request["payload"]["message"], "ikinci yasa nedir?");
    assert_eq!(request["payload"]["asker_role"], "teacher");
    // Byte-level: a bare `"teacher"`, not an object-wrapped or capitalised
    // enum — a re-tagged role would still parse as JSON above.
    let text = String::from_utf8(bytes).expect("the frame body is UTF-8 JSON");
    assert!(text.contains(r#""asker_role":"teacher""#), "{text}");

    let id = request["id"].as_str().expect("trace id").to_string();
    raw::answer(
        send,
        format!(
            r#"{{"status":"ok","id":"{id}","school":"{SCHOOL}","payload":{{"text":"F = ma"}}}}"#
        )
        .as_bytes(),
    )
    .await;
}

// ------------------------------------------------------------- api reads --
//
// The other direction: a service opens its own stream and asks the school API
// a question. Its request and the backend's answer are a published contract in
// exactly the same way as the frames above, so they are built and read here as
// bytes — a renamed field or a re-tagged outcome breaks every foreign service
// and must break this file first.

/// A bridge whose api-read path is armed by a real router, plus a seeded
/// student's id. Holding the router is what `build_router` arms, so it is
/// returned rather than dropped.
async fn armed(bridge: &AiBridge) -> (axum::Router, String) {
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;
    let cookie = common::login_as(&app, &db, "ayse", "student").await;
    let student = common::me_id(&app, &cookie).await;
    (app, student)
}

#[tokio::test]
async fn the_api_request_field_names_are_the_published_literals() {
    // Every documented field spelled out by hand: a rename of `on_behalf_of`
    // or `query` would leave a service sending a field the backend ignores,
    // which fails silently as "the AI answered about the wrong person".
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;
    let (_app, student) = armed(&bridge).await;

    let (answer, bytes) = raw::api_read(
        &service.conn,
        format!(
            r#"{{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","school":"{SCHOOL}","path":"/notes","query":"limit=1&offset=0","on_behalf_of":"{student}","method":"GET"}}"#
        )
        .as_bytes(),
    )
    .await;

    assert_eq!(
        raw::keys(&answer),
        ["body", "id", "outcome", "school", "status"],
        "api answer shape changed: {answer}"
    );
    assert_eq!(answer["outcome"], "ok");
    assert_eq!(answer["status"], 200);
    assert_eq!(
        answer["id"], "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "the trace id is echoed"
    );
    assert!(answer["body"]["items"].is_array(), "{answer}");
    // Byte level: the tag key is `outcome` and its value a bare lowercase
    // literal, not an object-wrapped or capitalised variant name.
    let text = String::from_utf8(bytes).expect("the answer frame is UTF-8 JSON");
    assert!(text.contains(r#""outcome":"ok""#), "{text}");
    assert!(text.contains(r#""status":200"#), "{text}");
}

#[tokio::test]
async fn only_id_and_path_are_required_of_an_api_request() {
    // `query`, `on_behalf_of` and `method` are optional by contract: a service
    // that sends neither must read as the service itself, over GET.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;
    let (_app, _student) = armed(&bridge).await;

    let (answer, _) = raw::api_read(
        &service.conn,
        br#"{"id":"trace-1","school":"demo","path":"/auth/me","unknown_field":true}"#,
    )
    .await;
    assert_eq!(answer["outcome"], "ok", "{answer}");
    assert_eq!(answer["status"], 200);
    // The synthetic principal the bridge runs as when nobody is named.
    assert_eq!(answer["body"]["role"], "ai", "{answer}");
}

#[tokio::test]
async fn the_api_refusal_frame_carries_exactly_the_published_keys_and_codes() {
    // A service switches on these code literals. The `method` cases also pin
    // the refusal *order*: a non-GET on a path that is not allowlisted either
    // is `method_not_allowed`, because the method is judged first.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;
    let (_app, _student) = armed(&bridge).await;

    let cases = [
        (
            "method_not_allowed",
            r#"{"id":"t1","school":"demo","path":"/notes","method":"DELETE"}"#.to_string(),
        ),
        (
            "method_not_allowed",
            // Lowercase is not the method: the literal is exactly `GET`.
            r#"{"id":"t2","school":"demo","path":"/notes","method":"get"}"#.to_string(),
        ),
        (
            "path_not_allowed",
            r#"{"id":"t3","school":"demo","path":"/courses"}"#.to_string(),
        ),
        (
            // Order pin: both refusals apply, the method one wins.
            "method_not_allowed",
            r#"{"id":"t4","school":"demo","path":"/nope","method":"POST"}"#.to_string(),
        ),
        (
            "unknown_user",
            r#"{"id":"t5","school":"demo","path":"/auth/me","on_behalf_of":"nobodyatall"}"#
                .to_string(),
        ),
        (
            "malformed",
            r#"{"id":"t6","school":"demo","path":42}"#.to_string(),
        ),
        // A frame that names no school at all: required, never defaulted.
        ("malformed", r#"{"id":"t7","path":"/auth/me"}"#.to_string()),
        // A string that is no slug — the service's own bug, not a missing
        // customer, so it is `malformed` rather than `unknown_school`.
        (
            "malformed",
            r#"{"id":"t8","school":"NOT A SLUG","path":"/auth/me"}"#.to_string(),
        ),
        // A well-formed slug this deployment does not serve.
        (
            "unknown_school",
            r#"{"id":"t9","school":"nope","path":"/auth/me"}"#.to_string(),
        ),
    ];

    for (expected_code, request) in cases {
        let (answer, _) = raw::api_read(&service.conn, request.as_bytes()).await;
        assert_eq!(
            raw::keys(&answer),
            ["code", "id", "message", "outcome", "school"],
            "refusal shape changed for {expected_code}: {answer}"
        );
        assert_eq!(answer["outcome"], "err", "{answer}");
        assert_eq!(
            answer["code"], expected_code,
            "refusal code spelling changed: {answer}"
        );
        assert!(
            answer["message"].as_str().is_some_and(|m| !m.is_empty()),
            "message must be a string a service can log: {answer}"
        );
        // Even a frame the backend could not parse into a request echoes the
        // id, which is what the service correlates its own logs by.
        assert!(
            answer["id"].as_str().is_some_and(|i| i.starts_with('t')),
            "{answer}"
        );
    }
}

#[tokio::test]
async fn an_api_read_answers_a_named_user_with_that_users_own_data() {
    // The happy path a service actually uses: read an own-scoped endpoint as
    // the student who asked. The answer must be that student's row, not the
    // service's synthetic principal.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;
    let (_app, student) = armed(&bridge).await;

    let (answer, _) = raw::api_read(
        &service.conn,
        format!(
            r#"{{"id":"tme","school":"{SCHOOL}","path":"/auth/me","on_behalf_of":"{student}"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(answer["outcome"], "ok", "{answer}");
    assert_eq!(answer["status"], 200);
    assert_eq!(answer["body"]["id"], student, "{answer}");
    assert_eq!(answer["body"]["role"], "student", "{answer}");
}

// --------------------------------------------------------------- tenancy --
//
// `hab/2`'s whole reason for existing: one shared fleet of services, every
// frame naming the school it means. These drive that through the raw client,
// because the refusal codes and the school echo are published contract exactly
// like the frame keys above.

/// An armed bridge over a deployment with a *second* school, `beta`, holding a
/// user of the same name as the demo school's. Returns the router (holding it
/// is what keeps the bridge armed), the registry, and the two ids of `ayse`.
async fn two_schools(bridge: &AiBridge) -> (axum::Router, Tenants, String, String) {
    let (app, demo_db, tenants) = common::app_with_ai_tenants(Some(bridge.clone())).await;
    let beta_db = tenants
        .create(
            &Slug::try_new("beta").unwrap(),
            "Beta College",
            ModuleSet::all(),
        )
        .await
        .expect("a second school");

    let demo_cookie = common::login_as(&app, &demo_db, "ayse", "student").await;
    let beta_cookie = common::login_as_school(&app, &beta_db, "beta", "ayse", "student").await;
    let demo_ayse = common::me_id(&app, &demo_cookie).await;
    let beta_ayse = common::me_id(&app, &beta_cookie).await;
    assert_ne!(
        demo_ayse, beta_ayse,
        "two schools, two separate `ayse` rows"
    );
    (app, tenants, demo_ayse, beta_ayse)
}

#[tokio::test]
async fn an_api_read_answers_out_of_the_school_the_frame_named() {
    // The isolation the field buys, proved both ways with one username: each
    // school's `ayse` is reachable in her own school and *nowhere else*. A
    // bridge that resolved the principal against the wrong database would
    // answer one of the cross reads with a user instead of `unknown_user`.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;
    let (_app, _tenants, demo_ayse, beta_ayse) = two_schools(&bridge).await;

    for (school, who) in [("demo", &demo_ayse), ("beta", &beta_ayse)] {
        let (answer, _) = raw::api_read(
            &service.conn,
            format!(
                r#"{{"id":"t-own","school":"{school}","path":"/auth/me","on_behalf_of":"{who}"}}"#
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(answer["outcome"], "ok", "{answer}");
        assert_eq!(answer["school"], school, "the school is echoed: {answer}");
        assert_eq!(answer["body"]["id"], who.as_str(), "{answer}");
        assert_eq!(answer["body"]["username"], "ayse");
    }

    // And crossed over: the other school's id names nobody here.
    for (school, who) in [("demo", &beta_ayse), ("beta", &demo_ayse)] {
        let (answer, _) = raw::api_read(
            &service.conn,
            format!(
                r#"{{"id":"t-cross","school":"{school}","path":"/auth/me","on_behalf_of":"{who}"}}"#
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(
            answer["code"], "unknown_user",
            "a user id from another school resolved in `{school}`: {answer}"
        );
    }
}

#[tokio::test]
async fn a_suspended_school_is_refused_with_its_own_code() {
    // Distinct from `unknown_school` on purpose: this deployment does serve
    // `beta`, it is switched off — the one refusal here worth retrying later.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("tutor", "chat.reply")).await;
    await_workers(&bridge, 1).await;
    let (_app, tenants, _demo_ayse, beta_ayse) = two_schools(&bridge).await;

    let read = format!(
        r#"{{"id":"t-susp","school":"beta","path":"/auth/me","on_behalf_of":"{beta_ayse}"}}"#
    );
    let (answer, _) = raw::api_read(&service.conn, read.as_bytes()).await;
    assert_eq!(answer["outcome"], "ok", "the school starts out active");

    tenants
        .set_status(&Slug::try_new("beta").unwrap(), SchoolStatus::Suspended)
        .await
        .expect("suspend beta");

    let (answer, _) = raw::api_read(&service.conn, read.as_bytes()).await;
    assert_eq!(answer["outcome"], "err", "{answer}");
    assert_eq!(answer["code"], "school_suspended", "{answer}");
    assert_eq!(
        answer["school"], "beta",
        "the refusal names the school back"
    );
    assert_eq!(answer["id"], "t-susp");

    // The demo school is untouched by its neighbour's suspension.
    let (answer, _) = raw::api_read(
        &service.conn,
        br#"{"id":"t-neighbour","school":"demo","path":"/auth/me"}"#,
    )
    .await;
    assert_eq!(answer["outcome"], "ok", "{answer}");
}

#[tokio::test]
async fn a_blob_read_names_its_school_too() {
    // The blob path resolves the school independently of the api path, and its
    // refusals are the same three codes — so it gets the same proof.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("indexer", "rag.index")).await;
    await_workers(&bridge, 1).await;
    let (_app, student, file, uploaded) = armed_with_file(&bridge).await;

    let (header, _, body) = raw::blob_read(
        &service.conn,
        format!(
            r#"{{"id":"t-b1","school":"{SCHOOL}","file":"{file}","on_behalf_of":"{student}"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(header["status"], "ok", "{header}");
    assert_eq!(body.len(), uploaded.len());

    for (school, code) in [("nope", "unknown_school"), ("NOT A SLUG", "malformed")] {
        let (header, _, body) = raw::blob_read(
            &service.conn,
            format!(
                r#"{{"id":"t-b2","school":"{school}","file":"{file}","on_behalf_of":"{student}"}}"#
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(header["status"], "err", "{header}");
        assert_eq!(header["code"], code, "{header}");
        assert!(body.is_empty(), "a refused blob stream carries no bytes");
    }

    // And with no school named at all the frame does not parse.
    let (header, _, _) = raw::blob_read(
        &service.conn,
        format!(r#"{{"id":"t-b3","file":"{file}","on_behalf_of":"{student}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(header["code"], "malformed", "{header}");
}

// --------------------------------------------------- blob stream contract --
//
// The bytes behind a course-note attachment, which no JSON frame can carry.
// The header frame is a published contract exactly like the ones above, and
// the `size`-then-EOF rule is what a foreign service implements by hand — so
// both are asserted here as bytes.

/// An armed router plus an uploaded course-note file: its id, and the bytes
/// that were uploaded, read back by a service that must receive them verbatim.
async fn armed_with_file(bridge: &AiBridge) -> (axum::Router, String, String, Vec<u8>) {
    let (app, db) = common::app_with_ai(Some(bridge.clone())).await;
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
    assert_eq!(res.status, 201, "{}", res.body);
    let note = common::id_of(&res.body);

    // Larger than one QUIC datagram, so the answer is a genuine multi-write
    // stream rather than something that happened to fit beside the header.
    let uploaded: Vec<u8> = (0..200 * 1024u32).map(|i| (i % 251) as u8).collect();
    let res = common::upload_file_at(
        &app,
        &teacher,
        &format!("/course-notes/{note}/files"),
        "recap.pdf",
        "application/pdf",
        &uploaded,
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.body);
    let file = common::id_of(&res.body);
    (app, student, file, uploaded)
}

#[tokio::test]
async fn the_blob_header_frame_carries_exactly_the_published_keys_then_size_bytes() {
    // Every documented field spelled out by hand, and the byte rule with it: a
    // service reads the header, then exactly `size` raw bytes, then EOF. A
    // length prefix sneaking in front of the body, or one byte too many after
    // it, would leave a foreign service parsing garbage.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("indexer", "rag.index")).await;
    await_workers(&bridge, 1).await;
    let (_app, student, file, uploaded) = armed_with_file(&bridge).await;

    let (header, bytes, body) = raw::blob_read(
        &service.conn,
        format!(
            r#"{{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","school":"{SCHOOL}","file":"{file}","on_behalf_of":"{student}"}}"#
        )
        .as_bytes(),
    )
    .await;

    assert_eq!(
        raw::keys(&header),
        ["content_type", "id", "name", "school", "size", "status"],
        "blob header shape changed: {header}"
    );
    assert_eq!(header["status"], "ok");
    assert_eq!(
        header["id"], "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "the trace id is echoed"
    );
    assert_eq!(header["name"], "recap.pdf");
    assert_eq!(header["content_type"], "application/pdf");
    assert_eq!(header["size"], uploaded.len(), "{header}");

    // Byte level: the tag key is `status` with a bare lowercase literal, and
    // `size` is a JSON number, not a string a service would have to parse.
    let text = String::from_utf8(bytes).expect("the header frame is UTF-8 JSON");
    assert!(text.contains(r#""status":"ok""#), "{text}");
    assert!(
        text.contains(&format!(r#""size":{}"#, uploaded.len())),
        "{text}"
    );

    // The body is raw: no four-byte length prefix, exactly `size` bytes, EOF.
    assert_eq!(body.len(), uploaded.len(), "`size` bytes then EOF");
    assert!(body == uploaded, "the bytes differ from what was uploaded");
}

#[tokio::test]
async fn a_blob_refusal_frame_carries_exactly_the_published_keys_and_no_bytes() {
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("indexer", "rag.index")).await;
    await_workers(&bridge, 1).await;
    let (_app, student, _file, _uploaded) = armed_with_file(&bridge).await;

    let (header, _, body) = raw::blob_read(
        &service.conn,
        format!(
            r#"{{"id":"t-blob","school":"{SCHOOL}","file":"01NOSUCHFILE","on_behalf_of":"{student}"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(
        raw::keys(&header),
        ["code", "id", "message", "school", "status"],
        "blob refusal shape changed: {header}"
    );
    assert_eq!(header["status"], "err");
    assert_eq!(header["code"], "not_found");
    assert_eq!(header["id"], "t-blob");
    assert!(body.is_empty(), "a refusal is followed by nothing at all");
}

#[tokio::test]
async fn an_api_read_still_answers_on_the_shared_client_stream_path() {
    // The discriminator guard. Both request shapes now arrive on the same
    // client-initiated streams and are told apart by their required field —
    // `path` for an api read, `file` for a blob. An api read must keep
    // answering exactly as it did before the blob shape existed, and a frame
    // that is neither must still be the api read's `malformed` refusal rather
    // than a silent hang.
    let bridge = bridge().await;
    let service = raw::handshake(&bridge, &raw::hello("indexer", "rag.index")).await;
    await_workers(&bridge, 1).await;
    let (_app, student, file, _uploaded) = armed_with_file(&bridge).await;

    let (answer, _) = raw::api_read(
        &service.conn,
        format!(
            r#"{{"id":"t-api","school":"{SCHOOL}","path":"/auth/me","on_behalf_of":"{student}"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(answer["outcome"], "ok", "{answer}");
    assert_eq!(answer["status"], 200);
    assert_eq!(answer["body"]["id"], student, "{answer}");

    // `path` wins over `file`: a frame carrying both is the api read it has
    // always been, so nothing that parsed before is re-routed now.
    let (answer, _) = raw::api_read(
        &service.conn,
        format!(
            r#"{{"id":"t-both","school":"{SCHOOL}","path":"/auth/me","file":"{file}","on_behalf_of":"{student}"}}"#
        )
        .as_bytes(),
    )
    .await;
    assert_eq!(answer["outcome"], "ok", "{answer}");
    assert_eq!(answer["status"], 200);

    // Neither shape: still the api read's refusal, with its `outcome` tag.
    let (answer, _) = raw::api_read(&service.conn, br#"{"id":"t-neither","school":"demo"}"#).await;
    assert_eq!(answer["outcome"], "err", "{answer}");
    assert_eq!(answer["code"], "malformed");
    assert_eq!(answer["id"], "t-neither");
}
