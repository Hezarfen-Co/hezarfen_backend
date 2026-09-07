//! What the request span is allowed to say — and, more importantly, what it is
//! not.
//!
//! Its own test binary because installing a tracer needs a *process-global*
//! `tracing` subscriber: the other suites run in the same process as each other
//! and would race for it (and would then export spans for every request every
//! other test makes).

mod common;

use std::collections::BTreeSet;

use common::{mem_app, send_raw};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Attribute keys that would carry personal data out of the process. KVKK
/// analysis fixed this list: telemetry names the school and the route, never
/// the person, their address, or what they sent.
const FORBIDDEN: &[&str] = &[
    "url.path",
    "url.full",
    "url.query",
    "client.address",
    "network.peer.address",
    "user_agent.original",
    "user.id",
    "user.name",
    "enduser.id",
    "cookie",
    "http.request.body",
    "http.response.body",
];

#[tokio::test]
async fn the_request_span_names_the_route_and_nothing_about_the_caller() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")))
        .init();

    let app = mem_app().await;
    let (_, headers, _) = send_raw(&app, "GET", "/health", None, None, Vec::new()).await;
    assert!(
        headers.get("x-request-id").is_some_and(|v| !v.is_empty()),
        "every response must carry the id its span was tagged with"
    );
    // An unrouted path must not become a span name — that is how a record id
    // ends up in a metric label.
    let _ = send_raw(&app, "GET", "/nope/42", None, None, Vec::new()).await;
    provider.force_flush().expect("flush");

    let spans = exporter.get_finished_spans().expect("exported spans");
    let names: BTreeSet<_> = spans.iter().map(|s| s.name.to_string()).collect();
    assert!(
        names.contains("GET /health"),
        "expected a span named after the METHOD and route template, got {names:?}"
    );
    assert!(
        names.contains("GET unmatched"),
        "an unrouted request is 'unmatched', never its URL: {names:?}"
    );

    let health = spans
        .iter()
        .find(|s| s.name == "GET /health")
        .expect("the health span");
    let attrs: Vec<(String, String)> = health
        .attributes
        .iter()
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect();
    assert!(
        attrs.contains(&("http.route".to_string(), "/health".to_string())),
        "{attrs:?}"
    );
    assert!(
        attrs
            .iter()
            .any(|(k, v)| k == "http.response.status_code" && v.parse::<u16>() == Ok(200)),
        "the status must be recorded, as a number: {attrs:?}"
    );
    assert!(
        attrs
            .iter()
            .any(|(k, v)| k == "http.request.id" && !v.is_empty()),
        "{attrs:?}"
    );

    for span in &spans {
        for kv in span.attributes.iter() {
            let key = kv.key.to_string();
            assert!(
                !FORBIDDEN.contains(&key.as_str()),
                "span {:?} carries {key:?}, which telemetry may never leave this process with",
                span.name
            );
            assert!(
                !key.starts_with("http.request.header.")
                    && !key.starts_with("http.response.header."),
                "span {:?} carries the header {key:?}",
                span.name
            );
        }
    }
}

/// The class of a refusal, on the span: `error.type` is the grouping key a
/// dashboard slices errors by, so it must be the stable name of the *kind* of
/// refusal (`not_found`) and never the status code or anything from the URL.
///
/// Own subscriber, scoped to this thread with `set_default` rather than
/// `init` — the process-global slot belongs to the guard test above, and a
/// second `init` in this binary would panic.
#[tokio::test]
async fn a_refusal_records_its_class_on_the_span() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    let _guard = tracing::subscriber::set_default(subscriber);

    let (app, db) = common::app_and_db().await;
    let cookie = common::login_as(&app, &db, "classteacher", "teacher").await;
    let (status, _, _) = send_raw(
        &app,
        "GET",
        "/notes/nosuchnote",
        Some(&cookie),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, 404);
    provider.force_flush().expect("flush");

    let spans = exporter.get_finished_spans().expect("exported spans");
    let span = spans
        .iter()
        .find(|s| s.name == "GET /notes/{id}")
        .unwrap_or_else(|| panic!("the note span, got {:?}", names(&spans)));
    assert_eq!(attr(span, "error.type").as_deref(), Some("not_found"));
    // A 404 is the caller's mistake, not ours: the span is not marked failed.
    assert_eq!(attr(span, "otel.status_code"), None);
}

/// A panicking handler answers the same 500 body every other internal error
/// answers, and its span says `panic` — not `500`, which is already there as
/// the status code.
///
/// Built from the two layers `build_router` wires (`lib.rs`: the
/// `CatchPanicLayer::custom(panic_response)` at :303 inside the `TraceLayer`
/// at :310) rather than driven through the real router, because no route in
/// the API panics and `Router::route` on an already-layered router adds a
/// route *outside* those layers — the assembled stack cannot be given a
/// panicking endpoint from a test without a test-only route in `build_router`.
#[tokio::test]
async fn a_panicking_handler_is_a_500_with_the_standard_body_and_a_panic_span() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    let _guard = tracing::subscriber::set_default(subscriber);

    let app: axum::Router = axum::Router::new()
        .route("/boom", axum::routing::get(boom))
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            hezarfen_backend::panic_response,
        ))
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(hezarfen_backend::request_span),
        );

    let (status, _, body) = send_raw(&app, "GET", "/boom", None, None, Vec::new()).await;
    assert_eq!(status, 500);
    // Byte-identical to `AppError::Internal`'s body: a panic is not a special
    // wire shape a client has to learn.
    assert_eq!(body, br#"{"error":"internal server error"}"#);
    provider.force_flush().expect("flush");

    let spans = exporter.get_finished_spans().expect("exported spans");
    let span = spans
        .iter()
        .find(|s| s.name == "GET /boom")
        .unwrap_or_else(|| panic!("the panicking span, got {:?}", names(&spans)));
    assert_eq!(attr(span, "error.type").as_deref(), Some("panic"));
}

async fn boom() -> &'static str {
    panic!("handler exploded")
}

fn names(spans: &[opentelemetry_sdk::trace::SpanData]) -> Vec<String> {
    spans.iter().map(|s| s.name.to_string()).collect()
}

fn attr(span: &opentelemetry_sdk::trace::SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}
