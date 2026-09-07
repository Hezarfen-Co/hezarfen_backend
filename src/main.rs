use anyhow::Context;
use hezarfen_backend::config::Config;
use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::UserRateLimiter;
use hezarfen_backend::state::{AppState, DbHealth};
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
    let db_up = DbHealth::default();
    // The control connection is the one every request touches (the school
    // lookup rides it), so it is the socket worth watching.
    keepalive(tenants.control().clone(), db_up.clone());
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
        db_up,
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

/// Ping the database forever so the WebSocket never sits idle long enough to
/// be dropped, and publish each verdict to `health` so the request guard can
/// refuse callers while the socket is down.
///
/// The ping needs its own deadline. A query issued while the socket is down
/// does not fail — the SDK parks it until the connection returns, so an
/// un-deadlined ping hangs exactly as long as the outage and never reports the
/// outage it exists to detect.
fn keepalive(db: Database, health: DbHealth) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            hezarfen_backend::constant::DB_KEEPALIVE_INTERVAL_SECS,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ping_timeout =
            std::time::Duration::from_secs(hezarfen_backend::constant::DB_PING_TIMEOUT_SECS);
        let metrics = hezarfen_backend::telemetry::Metrics::global();
        // The transitions are what an operator is paged about; a line per tick
        // would bury them. `None` until the first verdict, so that one always
        // announces itself.
        let mut was_up: Option<bool> = None;
        let mut down_since: Option<tokio::time::Instant> = None;
        loop {
            interval.tick().await;
            // corner-cut: an abandoned ping stays queued in the SDK and replays
            // when the socket heals, so a long outage lands a burst of no-op
            // `RETURN 1`s on recovery. Harmless; probe over a raw TCP dial
            // instead if that burst ever shows up in a profile.
            let started = tokio::time::Instant::now();
            let outcome = tokio::time::timeout(ping_timeout, db.query("RETURN 1")).await;
            let elapsed = started.elapsed();
            metrics.db_ping_duration.record(elapsed.as_secs_f64(), &[]);
            let up = match &outcome {
                Ok(Ok(_)) => {
                    tracing::debug!("database keepalive ping ok");
                    true
                }
                Ok(Err(err)) => {
                    tracing::debug!("database keepalive ping failed: {err}");
                    false
                }
                Err(_) => {
                    tracing::debug!("database keepalive ping timed out");
                    false
                }
            };
            health.set(up);
            metrics.db_up.record(u64::from(up), &[]);
            match (was_up, up) {
                (Some(true) | None, false) => {
                    down_since = Some(started);
                    tracing::warn!("database is down: the keepalive ping did not answer");
                }
                (Some(false), true) => {
                    let outage_secs = down_since
                        .take()
                        .map_or(0.0, |since| since.elapsed().as_secs_f64());
                    tracing::warn!(outage_secs, "database is back");
                }
                _ => {}
            }
            was_up = Some(up);
        }
    });
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
