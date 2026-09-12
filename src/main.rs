use anyhow::Context;
use hezarfen_backend::config::Config;
use hezarfen_backend::rate_limit::UserRateLimiter;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let telemetry_cfg = hezarfen_backend::config::telemetry_from_env()?;
    // Held until after graceful shutdown: dropping the guard flushes whatever
    // the exporters still have buffered.
    let (_telemetry, metrics) = hezarfen_backend::telemetry::init(&telemetry_cfg)?;
    match telemetry_cfg.otlp_endpoint.as_deref() {
        Some(endpoint) => {
            tracing::info!("exporting OpenTelemetry traces/metrics/logs to {endpoint}")
        }
        None => tracing::info!(
            "OpenTelemetry export is off (set OTEL_EXPORTER_OTLP_ENDPOINT to turn it on)"
        ),
    }

    let cfg = Config::from_env();
    // Hash the login decoy now, so the first unknown-username login is not the
    // one request that pays for it (see `PasswordHash::prewarm_decoy`).
    hezarfen_backend::domain::user::PasswordHash::prewarm_decoy();
    // Connects to the control database, migrates it and seeds the builder —
    // all of it idempotent and unconditional on every boot. School databases
    // come up lazily, one connection each, on first use.
    let tenants = database::init(&cfg).await?;
    tokio::fs::create_dir_all(&cfg.files_path)
        .await
        .with_context(|| format!("failed to create the files directory {}", cfg.files_path))?;
    let ai = hezarfen_backend::ai::start_bridge(&cfg).await?;
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: cfg.files_path.clone().into(),
        cookie_secure: cfg.cookie_secure,
        rate_limit: cfg.rate_limit.clone(),
        chatbot_limit: UserRateLimiter::per_user_minute(cfg.chatbot_per_minute),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai,
        metrics,
    });

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = tokio::net::TcpListener::bind(addr.as_str()).await?;
    tracing::info!("hezarfen_backend listening on http://{addr}");
    // `with_connect_info` records each connection's peer address, which the
    // rate limiter uses as its per-client key (unless TRUST_PROXY is set).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    tracing::info!("shutting down");
    Ok(())
}

/// Resolve on SIGINT (Ctrl-C) or SIGTERM. As PID 1 in a container the process
/// gets no default signal handling, so without this a `podman stop` waits out
/// its grace period and SIGKILLs us — dropping in-flight requests mid-write.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install SIGINT handler");
    };
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
