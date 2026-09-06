use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zapiska::config::Config;
use zapiska::db::pool;
use zapiska::db::repo::Repo;
use zapiska::github::{GitHubLookup, RealGitHub};
#[cfg(feature = "webmentions")]
use zapiska::http::reqwest_client;
use zapiska::http::{build_app, shutdown};
use zapiska::notify::NotificationBatcher;
use zapiska::state::AppState;
#[cfg(feature = "webmentions")]
use zapiska::worker;

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
    pool::run_migrations(&sqlite_pool, config.ip_hash_secret.as_deref())
        .expect("failed to run migrations");

    // A lost or rotated IP_HASH_SECRET silently splits IP-hash continuity:
    // stored hashes (comments, anyone-mode reaction identities) stop matching
    // freshly computed ones. Fail loud in the logs, not silent in the data.
    if config.ip_hash_secret.is_none() {
        let hashed_rows: i64 = sqlite_pool
            .get()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM comments WHERE submitter_ip_hash IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .ok()
            })
            .unwrap_or(0);
        if hashed_rows > 0 {
            tracing::warn!(
                rows = hashed_rows,
                "IP_HASH_SECRET is unset but the database holds salted IP hashes; \
                 restore the original secret from your .env backup or hashes will mismatch"
            );
        }
    }

    let bind_addr = config.bind_addr;

    let repo = Repo::new(sqlite_pool.clone());

    let notifier = Arc::new(NotificationBatcher::new(&config));
    let language = zapiska::language::LanguageGate::new(&config);

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
        notifier.clone(),
    );

    let state = AppState {
        config,
        pool: sqlite_pool,
        repo,
        github,
        notifier,
        language,
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
