//! What the request span is allowed to say — and, more importantly, what it is
//! not.
//!
//! Its own test binary because installing a tracer needs a *process-global*
//! `tracing` subscriber: the other suites run in the same process as each other
//! and would race for it (and would then export spans for every request every
//! other test makes).

mod common;

use std::collections::BTreeSet;

use axum::body::Body;
use common::{is_forbidden_key, mem_app, send_raw};
use hezarfen_backend::telemetry::Metrics;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::test]
async fn the_request_span_names_the_route_and_nothing_about_the_caller() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    // Events are exported too, as OTLP log records — the same rule applies to
    // them, so the same subscriber carries the appender bridge `telemetry::init`
    // installs in production.
    let logs = InMemoryLogExporter::default();
    let logger = SdkLoggerProvider::builder()
        .with_simple_exporter(logs.clone())
        .build();
    tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")))
        .with(OpenTelemetryTracingBridge::new(&logger))
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
                !is_forbidden_key(&key),
                "span {:?} carries {key:?}, which telemetry may never leave this process with",
                span.name
            );
        }
    }

    // The events: a login and a seeded account are the two places an account
    // name has historically leaked, so they are what is driven here.
    let (app, db) = common::app_and_db().await;
    let _ = common::login_as(&app, &db, LOGIN_USERNAME, "teacher").await;
    logger.force_flush().expect("flush logs");
    let records = logs.get_emitted_logs().expect("emitted logs");
    assert!(!records.is_empty(), "no log records were exported at all");
    for record in &records {
        for (key, value) in record.record.attributes_iter() {
            let key = key.to_string();
            assert!(
                !is_forbidden_key(&key),
                "a log record carries {key:?} = {value:?}"
            );
        }
        let body = format!("{:?}", record.record.body());
        assert!(
            !body.contains(LOGIN_USERNAME),
            "a log record body names the caller: {body}"
        );
    }
}

/// The account the log assertions above look for. Unusual enough that a match
/// in a log body is the account name and not a coincidence.
const LOGIN_USERNAME: &str = "telemetryprobeteacher";

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

/// A request whose future is dropped mid-flight — the client hung up, an outer
/// timeout fired — must still be counted out of `http.server.active_requests`.
/// Driven with a request body that never yields, so the handler is parked
/// inside the middleware when the future is dropped; a gauge that only
/// decrements after the `.await` stays at 1 forever here.
///
/// Its own meter provider (never the process-global one) so the sum is this
/// test's requests and nothing else's.
#[tokio::test]
async fn a_cancelled_request_is_counted_out_of_the_in_flight_gauge() {
    let exporter = InMemoryMetricExporter::default();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = Metrics::from_meter(meter_provider.meter("test"));
    let (app, _db) = common::app_with_metrics(metrics).await;

    // A body that is never going to produce its bytes: the `Json` extractor on
    // `POST /auth/login` parks on it.
    let body = Body::from_stream(futures_util::stream::pending::<
        Result<Vec<u8>, std::io::Error>,
    >());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/auth/login")
        .header("content-type", "application/json")
        .body(body)
        .expect("request");
    let pending = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        tower::ServiceExt::oneshot(app, req),
    )
    .await;
    assert!(pending.is_err(), "the parked request answered after all");

    meter_provider.force_flush().expect("flush metrics");
    let mut points = 0;
    let mut sum = 0i64;
    for resource in exporter.get_finished_metrics().expect("metrics") {
        for scope in resource.scope_metrics() {
            for metric in scope.metrics() {
                if metric.name() != "http.server.active_requests" {
                    continue;
                }
                if let AggregatedMetrics::I64(MetricData::Sum(data)) = metric.data() {
                    for point in data.data_points() {
                        points += 1;
                        sum += point.value();
                    }
                }
            }
        }
    }
    assert!(
        points > 0,
        "the gauge was never recorded — the test drove nothing"
    );
    assert_eq!(
        sum, 0,
        "the cancelled request is still counted as in flight"
    );
}

/// `http.request.id` lands on every span, so a caller-supplied one is only
/// echoed when it is a short, boring token; anything else is replaced with a
/// minted UUID. Without this a client writes free text — personal data, or an
/// unbounded label — onto the telemetry of every request it makes.
#[tokio::test]
async fn an_unusable_caller_supplied_request_id_is_replaced_with_a_uuid() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
    let _guard = tracing::subscriber::set_default(subscriber);

    let app = mem_app().await;
    let long = "x".repeat(70);
    let sent = |id: &str| {
        let app = app.clone();
        let id = id.to_string();
        async move {
            let req = axum::http::Request::builder()
                .method("GET")
                .uri("/health")
                .header("x-request-id", &id)
                .body(Body::empty())
                .expect("request");
            let res = tower::ServiceExt::oneshot(app, req)
                .await
                .expect("response");
            res.headers()
                .get("x-request-id")
                .expect("every response carries an id")
                .to_str()
                .expect("an ascii id")
                .to_string()
        }
    };

    let minted = sent(&long).await;
    assert_ne!(minted, long, "the 70-character id was echoed back");
    assert!(
        uuid::Uuid::parse_str(&minted).is_ok(),
        "an unusable id is replaced with a UUID, got {minted:?}"
    );
    let echoed = sent("ok-123").await;
    assert_eq!(echoed, "ok-123", "a boring id is the caller's to keep");

    provider.force_flush().expect("flush");
    let ids: Vec<String> = exporter
        .get_finished_spans()
        .expect("spans")
        .iter()
        .filter(|s| s.name == "GET /health")
        .filter_map(|s| attr(s, "http.request.id"))
        .collect();
    assert!(
        ids.contains(&minted) && ids.contains(&echoed),
        "the span must carry the id the response carried: {ids:?}"
    );
    assert!(
        !ids.contains(&long),
        "the rejected id reached a span anyway: {ids:?}"
    );
}
