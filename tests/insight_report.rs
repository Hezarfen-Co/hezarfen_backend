//! End-to-end tests for the school-report doors, against a fake ZEKA service.
//!
//! `POST /insights/runs/{run_day}/report` reads this school's own `zeka_*`
//! rows, dispatches them to the service that declares `insight.report`, stores
//! the returned HTML under the school's blob directory, and answers `200` with
//! what was stored; `GET` streams that document back. Everything runs over a
//! real QUIC socket on loopback — a real handshake, real TLS, real streams —
//! with [`Behaviour::Report`] standing in for what ZEKA does with the request.
//!
//! The assertions split the same way `tests/podcast.rs` splits them: what a
//! browser gets from the real router, and what the *service* received
//! ([`FakeService::seen`]) — a door that quietly sent nothing, or asked the
//! wrong principal, fails the `seen` assertions rather than looking green.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use hezarfen_backend::ai::protocol::{Greeting, Hello, Request, Response, read_frame, write_frame};
use hezarfen_backend::ai::{AiBridge, BridgeConfig};
use hezarfen_backend::constant::{
    AI_ALPN, AI_INSIGHT_REPORT_CAPABILITY, AI_PROTOCOL,
};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::tenant::DEMO_SCHOOL_ID;
use serde_json::{Value, json};
use uuid::Uuid;

const TOKEN: &str = "shared-ai-token";

/// One answer frame, **bounded**: a bridge that never answers must fail this
/// test by name in seconds rather than hang the run — a hung Test step holds
/// the serialized deploy lock behind it.
async fn frame_or_fail<T: serde::de::DeserializeOwned>(
    recv: &mut quinn::RecvStream,
    what: &str,
) -> T {
    match tokio::time::timeout(Duration::from_secs(10), read_frame(recv)).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(err)) => panic!("{what}: the frame could not be read: {err}"),
        Err(_) => panic!("{what}: no frame arrived within 10s — the bridge did not answer"),
    }
}

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

    /// The one report request this service was handed.
    fn report_request(&self) -> Request {
        self.seen()
            .into_iter()
            .find(|request| request.capability == AI_INSIGHT_REPORT_CAPABILITY)
            .expect("the door dispatched the report capability")
    }
}

/// What the fake service does with each request it receives.
#[derive(Clone)]
enum Behaviour {
    /// Answer like ZEKA's report package: a self-contained HTML document that
    /// echoes the run day and how many summaries it was handed.
    Report,
    /// Refuse like ZEKA's report handler does: the code is chosen by the run
    /// day, so one connection can drive every refusal class.
    Refuse,
    /// Answer `200` with an html member that is the empty string — a service
    /// defect the door must not persist.
    Empty,
}

/// The refusal code the fake service answers for a run day — all four codes
/// the real handler closes, one day each.
fn refusal_code(run_day: &str) -> String {
    match run_day {
        "2026-09-11" => "bad_request",
        "2026-09-12" => "insufficient_rows",
        "2026-09-13" => "document_too_large",
        "2026-09-14" => "internal",
        other => panic!("the fake service was asked for an unmapped day `{other}`"),
    }
    .to_string()
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
                    Behaviour::Report => Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload: report_answer(&request),
                    },
                    Behaviour::Refuse => {
                        let code = refusal_code(
                            request.payload["run_day"].as_str().unwrap_or_default(),
                        );
                        Response::Err {
                            id: request.id.clone(),
                            school: request.school.clone(),
                            message: format!("`{code}` reddedildi"),
                            code,
                        }
                    }
                    Behaviour::Empty => Response::Ok {
                        id: request.id.clone(),
                        school: request.school.clone(),
                        payload: json!({
                            "kind": "okul",
                            "run_day": request.payload["run_day"],
                            "format": "html",
                            "html": "",
                            "byte_size": 0,
                            "truncated": false,
                            "notes": [],
                        }),
                    },
                };
                let _ = write_frame(&mut send, &response).await;
                let _ = send.finish();
                let _ = send.stopped().await;
            });
        }
    });
}

/// The HTML a fake report render produces for the request it was handed.
fn report_html(request: &Request) -> String {
    let run_day = request.payload["run_day"].as_str().unwrap_or("<gun yok>");
    let students = request.payload["summaries"]
        .as_array()
        .map(|rows| rows.len())
        .unwrap_or(0);
    format!(
        "<!doctype html>\n<html lang=\"tr\"><head><meta charset=\"utf-8\">\
         <title>Okul analiz raporu</title></head>\
         <body><h1>Okul analiz raporu — {run_day}</h1>\
         <p>{students} öğrenci özeti</p></body></html>"
    )
}

/// What the fake ZEKA report package answers: the document plus its own
/// bookkeeping.
fn report_answer(request: &Request) -> Value {
    let html = report_html(request);
    json!({
        "kind": "okul",
        "run_day": request.payload["run_day"],
        "format": "html",
        "byte_size": html.len(),
        "html": html,
        "truncated": false,
        "notes": ["koşu defteri okundu"],
    })
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

/// A registered ZEKA service, the router wired to its bridge, a logged-in
/// manager, and the school's database handle — the fixture the round-trip
/// tests start from.
async fn report_app() -> (FakeService, Router, String, Database) {
    let bridge = bridge().await;
    let service = connect_service(
        &bridge,
        hello("zeka", &[AI_INSIGHT_REPORT_CAPABILITY]),
        Behaviour::Report,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge)).await;
    let manager = common::login_as(&app, &db, "mudur", "manager").await;
    (service, app, manager, db)
}

// --------------------------------------------------------------- fixtures --

/// Seed one student's ZEKA rows the way the service writes them: a summary
/// with one attention item, one live card, one segment profile, and the
/// ledger row for `run_day` with its pending student and failed module. All of
/// it is what the report dispatch must carry.
async fn seed_report_rows(db: &Database, run_day: &str, student: &str, audience: &str) {
    let student_uuid = Uuid::parse_str(student).expect("student id");
    let audience_uuid = Uuid::parse_str(audience).expect("audience id");
    let now = Timestamp::now().as_millis();

    sqlx::query(
        "INSERT INTO zeka_student_summary
             (student, marks, attendance, submission, study, confidence, computed_at, retain_until)
         VALUES ($1, '{\"courses\":{\"matematik\":{\"average\":72}}}'::jsonb, NULL, NULL, NULL,
                 'stable', $2, $3)",
    )
    .bind(student_uuid)
    .bind(now)
    .bind(now + 400 * 86_400_000)
    .execute(db)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO zeka_attention_item
             (student, trigger, course, fact, window_from, window_to, evidence, ord)
         VALUES ($1, 'attendance', NULL, 'son 30 gunun 4 dersi kacirildi', $2, $3,
                 '{\"absent_days\":4}'::jsonb, 1)",
    )
    .bind(student_uuid)
    .bind(now - 30 * 86_400_000)
    .bind(now)
    .execute(db)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO zeka_recommendation
             (id, audience, product, rule_id, rule_version, scope, about, audience_role,
              course, evidence, confidence, created_at, expires_at, retain_until,
              dismissed_at, dismissed_by, dismiss_reason)
         VALUES ($1, $2, 'T4', 'T4.attendance', 1, NULL, $3, 'teacher', NULL,
                 '{\"absent_days\":4,\"limitation\":\"yalniz ders yoklamasi\"}'::jsonb,
                 'stable', $4, $5, $6, NULL, NULL, NULL)",
    )
    .bind(Uuid::now_v7())
    .bind(audience_uuid)
    .bind(student_uuid)
    .bind(now)
    .bind(now + 86_400_000)
    .bind(now + 90 * 86_400_000)
    .execute(db)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO zeka_student_segment_profile
             (student, dimension, label, n_answers, n_correct, accuracy,
              overall_n_answers, overall_accuracy, contrast, confidence,
              computed_at, retain_until)
         VALUES ($1, 'bilissel_talep', 'analiz', 120, 48, 0.40, 240, 0.55, -0.15,
                 'stable', $2, $3)",
    )
    .bind(student_uuid)
    .bind(now)
    .bind(now + 400 * 86_400_000)
    .execute(db)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO zeka_run
             (run_day, started_at, finished_at, status, duration_ms,
              students_total, students_ok, students_failed, students_skipped,
              rows_written, budget_exceeded, budget_ms, retain_until)
         VALUES ($1, $2, $3, 'ok', 61000, 1, 1, 0, 0, 44, false, 60000, $4)",
    )
    .bind(run_day)
    .bind(now - 61_000)
    .bind(now)
    .bind(now + 90 * 86_400_000)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO zeka_run_pending (run, student, ord) VALUES ($1, $2, 1)")
        .bind(run_day)
        .bind(student_uuid)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO zeka_run_failed_module (run, module, ord) VALUES ($1, 'study', 1)")
        .bind(run_day)
        .execute(db)
        .await
        .unwrap();
}

// ------------------------------------------------------------- http doors --

/// One `POST /insights/runs/{run_day}/report`.
async fn generate(app: &Router, cookie: &str, run_day: &str) -> common::Res {
    common::send(
        app,
        "POST",
        &format!("/insights/runs/{run_day}/report"),
        Some(cookie),
        None,
    )
    .await
}

/// One `GET /insights/runs/{run_day}/report`, raw — the body is an HTML
/// document or a JSON refusal, never a parsed envelope.
async fn fetch_report(
    app: &Router,
    cookie: &str,
    run_day: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    common::send_raw(
        app,
        "GET",
        &format!("/insights/runs/{run_day}/report"),
        Some(cookie),
        None,
        Vec::new(),
    )
    .await
}

/// A refusal body parsed out of raw bytes.
fn refusal_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("a refusal body is JSON")
}

// ------------------------------------------------------------------ tests --

/// The whole round trip: the manager generates the document, the service is
/// handed the school's own rows and the caller's identity, and the stored
/// artifact is served back with the document's content type — byte for byte
/// what the service rendered.
#[tokio::test]
async fn a_generated_report_is_stored_and_served_to_its_manager() {
    let (service, app, manager, db) = report_app().await;
    let manager_id = common::me_id(&app, &manager).await;
    let student = common::login(&app, "ali").await;
    let student_id = common::me_id(&app, &student).await;
    let class_id = common::create_class(&app, &manager, "8-A", json!({})).await;
    seed_report_rows(&db, "2026-09-16", &student_id, &manager_id).await;

    let res = generate(&app, &manager, "2026-09-16").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["run_day"], "2026-09-16");
    assert_eq!(res.body["truncated"], false);
    assert_eq!(res.body["notes"], json!(["koşu defteri okundu"]));
    assert!(res.body["generated_at"].as_i64().unwrap() > 0);
    assert_eq!(
        res.body["byte_size"].as_u64().unwrap(),
        report_html(&service.report_request()).len() as u64,
        "the receipt sizes the stored document, not the service's claim"
    );

    // What the service was handed. This is the payload contract: the rows the
    // backend holds, the caller's identity, and the school's own fields.
    let request = service.report_request();
    assert_eq!(request.payload["kind"], "okul");
    assert_eq!(request.payload["run_day"], "2026-09-16");
    assert_eq!(
        request.payload["requested_by"], manager_id,
        "the dispatch names the authenticated caller, not the school"
    );
    assert!(request.payload["school"].get("slug").is_none(), "the report school has no slug");
    assert_eq!(request.payload["school"]["id"], DEMO_SCHOOL_ID);
    assert_eq!(request.payload["school"]["name"], "Demo School");
    assert!(
        request.payload["school"]["name"]
            .as_str()
            .is_some_and(|name| !name.is_empty()),
        "the school's display name travels"
    );
    // The class (şube) lookup: the summaries' evidence carries class ids, and
    // the document must print the name a reader recognizes.
    let classes = request.payload["classes"]
        .as_array()
        .expect("classes is a list");
    let seeded = classes
        .iter()
        .find(|row| row["id"] == class_id.as_str())
        .expect("the school's own class is in the payload");
    assert_eq!(seeded["name"], "8-A");
    let summaries = request.payload["summaries"]
        .as_array()
        .expect("summaries is a list");
    let mine = summaries
        .iter()
        .find(|row| row["student"] == student_id)
        .expect("the seeded student's summary is in the payload");
    assert_eq!(mine["confidence"], "stable");
    assert_eq!(mine["attention"][0]["trigger"], "attendance");
    assert_eq!(mine["attention"][0]["evidence"]["absent_days"], 4);
    let cards = request.payload["recommendations"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["rule_id"], "T4.attendance");
    assert_eq!(cards[0]["about"], student_id);
    assert_eq!(request.payload["profiles"][0]["dimension"], "bilissel_talep");
    assert_eq!(request.payload["profiles"][0]["contrast"], -0.15);
    let runs = request.payload["runs"].as_array().unwrap();
    assert_eq!(runs[0]["run_day"], "2026-09-16");
    assert_eq!(runs[0]["pending_students"], json!([student_id]));
    assert_eq!(runs[0]["failed_modules"], json!(["study"]));

    // The artifact a browser gets back.
    let (status, headers, bytes) = fetch_report(&app, &manager, "2026-09-16").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");
    let html = String::from_utf8(bytes).expect("the stored document is UTF-8");
    assert_eq!(html, report_html(&service.report_request()));
    assert!(html.starts_with("<!doctype html>"));
}

/// Generation is deliberately synchronous and does not require a ledger row:
/// a run day with no `zeka_run` row still dispatches (the service says in
/// `notes` what it found), and the artifact it returns is served. A `404`
/// here would refuse a day the POST door had just answered `200` for.
#[tokio::test]
async fn a_run_day_with_no_ledger_row_still_generates_and_serves() {
    let (service, app, manager, _db) = report_app().await;

    let res = generate(&app, &manager, "2099-01-01").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["run_day"], "2099-01-01");

    let request = service.report_request();
    assert_eq!(request.payload["run_day"], "2099-01-01");
    assert_eq!(
        request.payload["runs"].as_array().map(Vec::len),
        Some(0),
        "no ledger row means no ledger rows, not a refusal"
    );

    let (status, headers, _bytes) = fetch_report(&app, &manager, "2099-01-01").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
}

/// Reading before anything was generated is the coded `409` — the refusal a
/// client branches on, never a `404` it could confuse with a missing route.
#[tokio::test]
async fn a_report_that_was_never_generated_is_a_coded_conflict() {
    let (app, db) = common::app_and_db().await;
    let manager = common::login_as(&app, &db, "mudur", "manager").await;

    let (status, _headers, bytes) = fetch_report(&app, &manager, "2099-12-31").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let body = refusal_body(&bytes);
    assert_eq!(body["code"], "report_missing");
    assert!(
        body["error"].as_str().unwrap().contains("2099-12-31"),
        "the message names the day: {body}"
    );
}

/// A service that answers, refusing, maps each of its four codes to the right
/// HTTP class — and stores nothing on any of them.
#[tokio::test]
async fn service_refusals_map_each_code_to_its_http_class() {
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("zeka", &[AI_INSIGHT_REPORT_CAPABILITY]),
        Behaviour::Refuse,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge)).await;
    let manager = common::login_as(&app, &db, "mudur", "manager").await;

    // `bad_request`: the service judged the composed request bad.
    let res = generate(&app, &manager, "2026-09-11").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // `insufficient_rows`: nothing to render — a coded conflict, not a 500.
    let res = generate(&app, &manager, "2026-09-12").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "report_empty");
    assert!(
        res.body["error"]
            .as_str()
            .is_some_and(|message| message.contains("insufficient_rows")),
        "the service's own words survive: {}",
        res.body
    );

    // `document_too_large`: the render happened but is over the ceiling.
    let res = generate(&app, &manager, "2026-09-13").await;
    assert_eq!(res.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", res.body);

    // `internal`: the render failed on the service's side.
    let res = generate(&app, &manager, "2026-09-14").await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", res.body);

    // None of the four stored a document.
    for day in ["2026-09-11", "2026-09-12", "2026-09-13", "2026-09-14"] {
        let (status, _headers, bytes) = fetch_report(&app, &manager, day).await;
        assert_eq!(status, StatusCode::CONFLICT, "{day}");
        assert_eq!(refusal_body(&bytes)["code"], "report_missing", "{day}");
    }
}

/// No service at all: `503` before any read, and nothing was stored.
#[tokio::test]
async fn generating_without_a_service_is_unavailable() {
    let (app, db) = common::app_and_db().await;
    let manager = common::login_as(&app, &db, "mudur", "manager").await;

    let res = generate(&app, &manager, "2026-09-14").await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
    assert!(res.body["error"].is_string());
}

/// A service that answers `200` with an empty document is refused: a blank
/// page must never be stored and served as if it were the report.
#[tokio::test]
async fn an_empty_document_is_never_stored() {
    let bridge = bridge().await;
    let _service = connect_service(
        &bridge,
        hello("zeka", &[AI_INSIGHT_REPORT_CAPABILITY]),
        Behaviour::Empty,
    )
    .await;
    await_workers(&bridge, 1).await;
    let (app, db) = common::app_with_ai(Some(bridge)).await;
    let manager = common::login_as(&app, &db, "mudur", "manager").await;

    let res = generate(&app, &manager, "2026-09-10").await;
    assert_eq!(res.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", res.body);

    let (status, _headers, bytes) = fetch_report(&app, &manager, "2026-09-10").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refusal_body(&bytes)["code"], "report_missing");
}

/// Both doors are manager+; a teacher is refused before anything is read or
/// dispatched, and a malformed day never shapes a path.
#[tokio::test]
async fn report_doors_are_manager_only_and_validate_the_day() {
    let (app, db) = common::app_and_db().await;
    let teacher = common::login_as(&app, &db, "ayse", "teacher").await;

    let res = generate(&app, &teacher, "2026-09-16").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let (status, _headers, _bytes) = fetch_report(&app, &teacher, "2026-09-16").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let manager = common::login_as(&app, &db, "mudur", "manager").await;
    let res = generate(&app, &manager, "2026-9-16").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // A percent-encoded traversal in the day segment decodes to something no
    // `YYYY-MM-DD` day is, so it is refused before it could shape a path.
    let (status, _headers, bytes) = fetch_report(&app, &manager, "%2e%2e%2f%2e%2e%2fetc%2fpasswd").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a path-shaped day is refused before it is joined: {}",
        String::from_utf8_lossy(&bytes)
    );
}
