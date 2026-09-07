//! Observability bootstrap: tracing subscriber, OTLP export and the process's
//! metric instruments.
//!
//! **Privacy is the design constraint, not a caveat.** Legal analysis (KVKK)
//! fixed the rule that telemetry leaving this process carries no user
//! identity, no client address, no URL path (which holds record ids), no
//! request or response bodies, and no headers or cookies. What may leave: the
//! route *template* (`/notes/{id}`), the HTTP method, the status code, the
//! school slug and a random per-request id. Every attribute added anywhere in
//! this crate must fit that list — see the guard test in `tests/telemetry.rs`,
//! which fails the build if a forbidden key ever shows up on a span.
//!
//! Export is off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set (the same
//! "off unless configured" shape as `AI_QUIC_ADDR`). With it unset the process
//! logs exactly as it always did and every instrument below is a noop.

use std::sync::{Arc, OnceLock};

use opentelemetry::metrics::{Counter, Gauge, Histogram, UpDownCounter};
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Instrumentation scope name — the OTLP `scope.name` on every metric below.
const SCOPE: &str = "hezarfen_backend";

/// How stdout logs are rendered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable lines, unchanged from before OpenTelemetry existed here.
    #[default]
    Pretty,
    /// One JSON object per event, with the target, the span fields and (when
    /// OTLP is on) the `trace_id`, so a collector can join logs to traces.
    Json,
}

impl LogFormat {
    /// `LOG_FORMAT`, rejecting anything that is not a spelling we render —
    /// silently falling back would hand a log shipper a format it cannot parse
    /// and nobody would notice until the logs were needed.
    pub fn parse(value: Option<String>) -> Result<Self, TelemetryError> {
        match value.as_deref().map(str::trim) {
            None | Some("") => Ok(Self::Pretty),
            Some(v) if v.eq_ignore_ascii_case("pretty") => Ok(Self::Pretty),
            Some(v) if v.eq_ignore_ascii_case("json") => Ok(Self::Json),
            Some(other) => Err(TelemetryError::LogFormat(other.to_string())),
        }
    }
}

/// What this process was told about telemetry (see [`crate::config::Config`]).
/// Everything else — headers, protocol, sampler, service name — is read by the
/// OpenTelemetry SDK straight from its own standard variables; re-parsing them
/// here would only create a second, lying source of truth.
#[derive(Clone, Debug)]
pub struct TelemetryConfig {
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`. Presence alone turns export on; the SDK
    /// re-reads the value itself, so it is never passed to an exporter here.
    pub otlp_endpoint: Option<String>,
    pub log_format: LogFormat,
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("LOG_FORMAT={0:?} is not one of: pretty, json")]
    LogFormat(String),
    #[error("OTEL_EXPORTER_OTLP_PROTOCOL={0:?} is not supported; use grpc or http/protobuf")]
    Protocol(String),
    #[error("could not build the OTLP {signal} exporter: {source}")]
    Exporter {
        signal: &'static str,
        #[source]
        source: opentelemetry_otlp::ExporterBuildError,
    },
}

/// Keeps the export pipelines alive and drains them on the way out. Dropping it
/// flushes whatever is still buffered — hold it in `main` until *after*
/// graceful shutdown, or the last requests of a deployment go unreported.
pub struct TelemetryGuard {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    logger: Option<SdkLoggerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Shutdown flushes; failures here are reported to stderr only, since
        // the subscriber may already be on its way down.
        if let Some(p) = self.tracer.take()
            && let Err(err) = p.shutdown()
        {
            eprintln!("telemetry: tracer shutdown failed: {err}");
        }
        if let Some(p) = self.meter.take()
            && let Err(err) = p.shutdown()
        {
            eprintln!("telemetry: meter shutdown failed: {err}");
        }
        if let Some(p) = self.logger.take()
            && let Err(err) = p.shutdown()
        {
            eprintln!("telemetry: logger shutdown failed: {err}");
        }
    }
}

/// Explicit histogram buckets for `http.server.request.duration`, in seconds —
/// the boundaries the HTTP semantic convention prescribes. Left to the SDK
/// default they would be the generic 0–10000 ladder, which puts every web
/// request this API serves into the first bucket.
const HTTP_DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// The handle [`init`] published, read by [`Metrics::global`].
static GLOBAL: OnceLock<Metrics> = OnceLock::new();

/// Every instrument this process publishes, declared once at boot and cloned
/// into whatever needs them. With no OTLP endpoint configured the global meter
/// provider is the SDK's noop, so each instrument is a cheap empty shell and
/// recording into one is a no-op — call sites never branch on "is telemetry on".
#[derive(Clone)]
pub struct Metrics {
    /// Server-side request latency in seconds
    /// (attrs: `http.request.method`, `http.route`,
    /// `http.response.status_code`, `school`).
    http_server_request_duration: Histogram<f64>,
    /// Requests currently in flight (attrs: `http.request.method`).
    http_server_active_requests: UpDownCounter<i64>,
    /// Error responses, by kind (attrs: `error.type`, `http.route`).
    pub errors_total: Counter<u64>,
    /// Panics caught by the panic layer.
    pub panics_total: Counter<u64>,
    /// Requests refused by the rate limiter (attr: `tier`).
    pub rate_limit_rejections_total: Counter<u64>,
    /// Database keepalive ping latency in seconds.
    pub db_ping_duration: Histogram<f64>,
    /// 1 when the last database ping answered, 0 when it did not.
    pub db_up: Gauge<u64>,
    /// AI services currently registered on the QUIC bridge.
    pub ai_workers: Gauge<u64>,
    /// AI requests dispatched and not yet answered.
    pub ai_requests_inflight: UpDownCounter<i64>,
    /// AI round-trip latency in seconds (attr: `capability`).
    pub ai_request_duration: Histogram<f64>,
    /// AI requests that hit their deadline.
    pub ai_request_timeouts_total: Counter<u64>,
    /// Rejected bridge handshakes (attr: `reason`).
    pub ai_handshake_failures_total: Counter<u64>,
    /// Open WebSockets (attr: `kind`, e.g. exam room or board).
    pub ws_connections: UpDownCounter<i64>,
    /// Schools currently held in the tenant connection cache.
    pub tenant_cache_size: Gauge<u64>,
}

impl Metrics {
    /// Declare every instrument against the global meter provider — the noop
    /// one unless [`init`] installed an OTLP pipeline first.
    pub fn new() -> Self {
        Self::from_meter(global::meter(SCOPE))
    }

    /// The same instruments against a caller-supplied meter. Exists for tests
    /// that want to read their own recordings back without claiming the
    /// process-global meter provider, which the whole test binary shares.
    pub fn from_meter(meter: opentelemetry::metrics::Meter) -> Self {
        Self {
            http_server_request_duration: meter
                .f64_histogram("http.server.request.duration")
                .with_unit("s")
                .with_description("Duration of inbound HTTP requests")
                .with_boundaries(HTTP_DURATION_BUCKETS.to_vec())
                .build(),
            http_server_active_requests: meter
                .i64_up_down_counter("http.server.active_requests")
                .with_description("Inbound HTTP requests currently in flight")
                .build(),
            errors_total: meter
                .u64_counter("errors_total")
                .with_description("Error responses served, by error type")
                .build(),
            panics_total: meter
                .u64_counter("panics_total")
                .with_description("Handler panics caught at the edge")
                .build(),
            rate_limit_rejections_total: meter
                .u64_counter("rate_limit_rejections_total")
                .with_description("Requests refused by a rate limit tier")
                .build(),
            db_ping_duration: meter
                .f64_histogram("db_ping_duration")
                .with_unit("s")
                .with_description("Database keepalive ping round trip")
                .build(),
            db_up: meter
                .u64_gauge("db_up")
                .with_description("1 when the last database ping answered")
                .build(),
            ai_workers: meter
                .u64_gauge("ai_workers")
                .with_description("AI services registered on the QUIC bridge")
                .build(),
            ai_requests_inflight: meter
                .i64_up_down_counter("ai_requests_inflight")
                .with_description("AI requests awaiting an answer")
                .build(),
            ai_request_duration: meter
                .f64_histogram("ai_request_duration")
                .with_unit("s")
                .with_description("AI request round trip, by capability")
                .build(),
            ai_request_timeouts_total: meter
                .u64_counter("ai_request_timeouts_total")
                .with_description("AI requests that hit their deadline")
                .build(),
            ai_handshake_failures_total: meter
                .u64_counter("ai_handshake_failures_total")
                .with_description("Rejected AI bridge handshakes, by reason")
                .build(),
            ws_connections: meter
                .i64_up_down_counter("ws_connections")
                .with_description("Open WebSockets, by kind")
                .build(),
            tenant_cache_size: meter
                .u64_gauge("tenant_cache_size")
                .with_description("Schools held in the tenant connection cache")
                .build(),
        }
    }

    /// Instruments bound to whatever meter provider is installed — the noop one
    /// in tests, where nothing calls [`init`].
    pub fn noop() -> Self {
        Self::new()
    }

    /// The process's instruments, for the call sites that exist before (or
    /// outside) [`crate::state::AppState`] — the rate limiter, built while the
    /// state is still being assembled, the keepalive task and the tenant cache.
    ///
    /// [`init`] publishes the handle it built; before that (every test suite,
    /// which never calls `init`) each call declares instruments against
    /// whatever meter provider is installed, so a test that installs its own
    /// provider first still sees what these call sites record. Only reached on
    /// a rejection or a cache change, never per request.
    pub fn global() -> Self {
        GLOBAL.get().cloned().unwrap_or_else(Self::new)
    }

    /// Count one request in; the returned guard value must be handed back to
    /// [`Metrics::request_finished`].
    pub fn request_started(&self, method: &str) {
        self.http_server_active_requests.add(
            1,
            &[KeyValue::new("http.request.method", method.to_string())],
        );
    }

    /// Count the request out and record how long it took. `school` is the slug
    /// only — never a user, never a path.
    pub fn request_finished(
        &self,
        method: &str,
        route: &str,
        status: u16,
        school: Option<&str>,
        elapsed: std::time::Duration,
    ) {
        self.http_server_active_requests.add(
            -1,
            &[KeyValue::new("http.request.method", method.to_string())],
        );
        let mut attrs = vec![
            KeyValue::new("http.request.method", method.to_string()),
            KeyValue::new("http.route", route.to_string()),
            KeyValue::new("http.response.status_code", i64::from(status)),
        ];
        if let Some(school) = school {
            attrs.push(KeyValue::new("school", school.to_string()));
        }
        self.http_server_request_duration
            .record(elapsed.as_secs_f64(), &attrs);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// A slot on the request for the school it turned out to belong to.
///
/// The metrics middleware runs before anything knows which school a caller is
/// in — the slug only becomes known when
/// [`crate::web::tenant_state::resolve_tenant`] reads the session cookie, deep
/// inside the handler, where the request has already been moved. So the
/// middleware puts an empty slot in the request's extensions on the way in and
/// reads it on the way out; `resolve_tenant` fills it in between. The slug is
/// the only thing that ever goes in here.
#[derive(Clone, Default)]
pub struct SchoolSlot(Arc<OnceLock<String>>);

impl SchoolSlot {
    /// Name the school for this request. Later calls are ignored — a request
    /// belongs to exactly one school.
    pub fn set(&self, slug: &str) {
        let _ = self.0.set(slug.to_string());
    }

    pub fn get(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }
}

/// Install the subscriber and, when an OTLP endpoint is configured, the export
/// pipelines. Call once, before anything logs.
pub fn init(cfg: &TelemetryConfig) -> Result<(TelemetryGuard, Metrics), TelemetryError> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hezarfen_backend=debug"));
    let fmt_layer = match cfg.log_format {
        LogFormat::Pretty => tracing_subscriber::fmt::layer().boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed(),
    };

    let Some(_endpoint) = cfg.otlp_endpoint.as_deref() else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        return Ok((
            TelemetryGuard {
                tracer: None,
                meter: None,
                logger: None,
            },
            publish(Metrics::new()),
        ));
    };

    let resource = resource();
    let protocol = Protocol::from_env()?;

    // Sampling is left to the SDK, which reads OTEL_TRACES_SAMPLER and
    // OTEL_TRACES_SAMPLER_ARG itself and defaults to parent-based always-on.
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(protocol.span_exporter()?)
        .build();
    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource.clone())
        .with_periodic_exporter(protocol.metric_exporter()?)
        .build();
    let logger_provider = SdkLoggerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(protocol.log_exporter()?)
        .build();

    let tracer = {
        use opentelemetry::trace::TracerProvider as _;
        tracer_provider.tracer(SCOPE)
    };
    global::set_tracer_provider(tracer_provider.clone());
    global::set_meter_provider(meter_provider.clone());

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        // Events become OTLP logs carrying the enclosing span's trace id, so a
        // collector can pivot from a log line to its trace.
        .with(
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                &logger_provider,
            ),
        )
        .init();

    Ok((
        TelemetryGuard {
            tracer: Some(tracer_provider),
            meter: Some(meter_provider),
            logger: Some(logger_provider),
        },
        publish(Metrics::new()),
    ))
}

/// Hand the freshly built instruments to [`Metrics::global`]. A second `init`
/// (only tests do that) keeps the first set rather than swapping it mid-flight.
fn publish(metrics: Metrics) -> Metrics {
    let _ = GLOBAL.set(metrics.clone());
    metrics
}

/// Which OTLP wire transport to use, from the standard
/// `OTEL_EXPORTER_OTLP_PROTOCOL`. Both are compiled in; the collector is
/// expected on localhost or a private network, so neither carries TLS.
enum Protocol {
    Grpc,
    HttpProtobuf,
}

impl Protocol {
    fn from_env() -> Result<Self, TelemetryError> {
        match std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            None | Some("") | Some("grpc") => Ok(Self::Grpc),
            Some("http/protobuf") => Ok(Self::HttpProtobuf),
            Some(other) => Err(TelemetryError::Protocol(other.to_string())),
        }
    }

    fn span_exporter(&self) -> Result<opentelemetry_otlp::SpanExporter, TelemetryError> {
        let builder = opentelemetry_otlp::SpanExporter::builder();
        match self {
            Self::Grpc => builder.with_tonic().build(),
            Self::HttpProtobuf => builder.with_http().build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "trace",
            source,
        })
    }

    fn metric_exporter(&self) -> Result<opentelemetry_otlp::MetricExporter, TelemetryError> {
        let builder = opentelemetry_otlp::MetricExporter::builder();
        match self {
            Self::Grpc => builder.with_tonic().build(),
            Self::HttpProtobuf => builder.with_http().build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "metric",
            source,
        })
    }

    fn log_exporter(&self) -> Result<opentelemetry_otlp::LogExporter, TelemetryError> {
        let builder = opentelemetry_otlp::LogExporter::builder();
        match self {
            Self::Grpc => builder.with_tonic().build(),
            Self::HttpProtobuf => builder.with_http().build(),
        }
        .map_err(|source| TelemetryError::Exporter {
            signal: "log",
            source,
        })
    }
}

/// What every exported signal says it came from. `OTEL_SERVICE_NAME` wins when
/// set (the SDK's own default detector reads it); `DEPLOY_ENV` names the
/// deployment, e.g. `staging`.
fn resource() -> Resource {
    let mut builder = Resource::builder().with_attributes([KeyValue::new(
        opentelemetry_semantic_conventions::resource::SERVICE_VERSION,
        env!("CARGO_PKG_VERSION"),
    )]);
    if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
        builder = builder.with_service_name(SCOPE);
    }
    if let Some(env_name) = std::env::var("DEPLOY_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        builder = builder.with_attribute(KeyValue::new(
            opentelemetry_semantic_conventions::resource::DEPLOYMENT_ENVIRONMENT_NAME,
            env_name,
        ));
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::{LogFormat, TelemetryError};

    #[tokio::test]
    async fn log_format_parses_the_two_spellings_and_rejects_the_rest() {
        assert_eq!(LogFormat::parse(None).unwrap(), LogFormat::Pretty);
        assert_eq!(
            LogFormat::parse(Some("  ".into())).unwrap(),
            LogFormat::Pretty
        );
        assert_eq!(
            LogFormat::parse(Some("pretty".into())).unwrap(),
            LogFormat::Pretty
        );
        assert_eq!(
            LogFormat::parse(Some(" JSON ".into())).unwrap(),
            LogFormat::Json
        );
        // Rejected, not defaulted: a shipper configured for JSON that silently
        // receives pretty lines fails at the far end, hours later.
        let err = LogFormat::parse(Some("logfmt".into())).unwrap_err();
        assert!(matches!(err, TelemetryError::LogFormat(v) if v == "logfmt"));
    }
}
