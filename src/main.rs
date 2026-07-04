use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zapiska::config::Config;
use zapiska::db::pool;
use zapiska::db::repo::CommentsRepo;
use zapiska::github::{GitHubLookup, RealGitHub};
use zapiska::http::reqwest_client;
use zapiska::http::{build_app, shutdown};
use zapiska::state::AppState;
use zapiska::worker;

#[tokio::main]
async fn main() {
    // Load .env if present (silently ignores if missing)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env().expect("failed to load configuration");
    info!(config = %config.redacted_display(), "server starting");

    let http_client = reqwest_client::build_client(&config);

    let sqlite_pool =
        pool::create_pool(&config.database_path).expect("failed to create SQLite pool");
    pool::run_migrations(&sqlite_pool).expect("failed to run migrations");

    let bind_addr = config.bind_addr;

    let repo = CommentsRepo::new(sqlite_pool.clone());

    let github: Arc<dyn GitHubLookup> = Arc::new(RealGitHub::new(
        repo.clone(),
        config.fetch_timeout_ms,
        config.github_token.clone(),
        http_client.clone(),
    ));

    let (wm_sender, wm_receiver) = worker::channel(config.worker_backlog);
    worker::spawn_worker_for_state(
        wm_receiver,
        repo.clone(),
        http_client.clone(),
        Arc::clone(&github),
        config.public_target_origin.clone(),
        config.max_content_len,
        config.fetch_timeout_ms,
    );

    let state = AppState {
        config,
        pool: sqlite_pool,
        repo,
        github,
        wm_sender,
        http_client,
    };

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
}
