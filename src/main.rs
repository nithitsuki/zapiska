use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zapiska::config::Config;
use zapiska::db::pool;
use zapiska::db::repo::CommentsRepo;
use zapiska::github::{GitHubLookup, RealGitHub};
#[cfg(feature = "webmentions")]
use zapiska::http::reqwest_client;
use zapiska::http::{build_app, shutdown};
use zapiska::state::AppState;
#[cfg(feature = "webmentions")]
use zapiska::worker;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Config::from_env().expect("failed to load configuration");
    info!(config = %config.redacted_display(), "server starting");

    // Build an HTTP client. With webmentions enabled, it uses the SSRF-safe
    // builder; without, a plain client suffices for GitHub lookup.
    #[cfg(feature = "webmentions")]
    let http_client = reqwest_client::build_client(&config);
    #[cfg(not(feature = "webmentions"))]
    let http_client = reqwest::Client::builder()
        .build()
        .expect("failed to build HTTP client");

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

    // Webmention worker — only spawned when the feature is enabled.
    #[cfg(feature = "webmentions")]
    let (wm_sender, wm_receiver) = worker::channel(config.worker_backlog);
    #[cfg(feature = "webmentions")]
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
        #[cfg(feature = "webmentions")]
        wm_sender,
        http_client,
        limiter: Arc::new(zapiska::state::Limiter::new()),
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
