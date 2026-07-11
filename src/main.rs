use hezarfen_backend::config::Config;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hezarfen_backend=debug".into()),
        )
        .init();

    let cfg = Config::from_env();
    let db = database::init(&cfg).await?;
    let app = build_router(AppState {
        db,
        cookie_secure: cfg.cookie_secure,
        rate_limit: cfg.rate_limit.clone(),
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
/// its grace period and SIGKILLs us — skipping the embedded store's clean close.
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
