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
