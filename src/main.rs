use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zapiska::config::Config;
use zapiska::http::{build_app, shutdown};
use zapiska::state::AppState;

#[tokio::main]
async fn main() {
    // `--version` / `-V` must not require configuration: operators check the
    // binary version before any `.env` exists.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("zapiska {}", zapiska::APP_VERSION);
        return;
    }

    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env().expect("failed to load configuration");
    info!(config = %config.redacted_display(), "server starting");

    let bind_addr = config.bind_addr;

    // All assembly (pool, migrations, gates, repo, notifier, worker) lives
    // in AppState::start — main only serves what it returns.
    let state = AppState::start(config).expect("failed to start");

    // Shutdown drain: after the server stops accepting connections, flush
    // open notification windows as final digests (same retry policy,
    // awaited) so a restart during a burst still alerts the admin.
    // Drained twice: handler senders die with the router at serve return,
    // but a webmention job already mid-flight can push during the first
    // drain — the second pass catches those stragglers. Residual
    // best-effort: a job completing after the second pass's checks is not
    // awaited (narrow: it must finish inside the second drain's tail).
    let drain_notifier = Arc::clone(&state.notifier);
    let drain_client = state.http_client.clone();

    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .expect("failed to bind address");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown::shutdown_signal())
    .await
    .expect("server exited with error");
    drain_notifier.drain(&drain_client).await;
    drain_notifier.drain(&drain_client).await;
}
