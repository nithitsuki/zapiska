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

    if config.telegram_bot_token.is_some() != config.telegram_chat_id.is_some() {
        tracing::warn!(
            "Telegram notifications are half-configured (need both TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID); Telegram delivery is disabled"
        );
    }

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

    // Fail loud on corruption: a database that fails quick_check must not
    // serve traffic (and /healthz would report 503 for it anyway).
    if config.db_quick_check {
        check_db_quick(&sqlite_pool).expect("database integrity check failed");
    }

    // A non-loopback bind is directly reachable from the network: TLS must
    // terminate at the edge (the __Host- admin cookie requires HTTPS). The
    // proxy-identity half of the warning depends on TRUST_PROXY (see
    // src/http/peer.rs). Warning-only — never fail boot for a listen address.
    if is_public_bind(&config.bind_addr) {
        tracing::warn!(
            bind_addr = %config.bind_addr,
            "{}",
            public_bind_warning(config.trust_proxy)
        );
    }

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

/// True when the server listens on a non-loopback address, i.e. it is
/// directly reachable from the network rather than only via a local reverse
/// proxy. Used for the startup proxy-config warning only.
fn is_public_bind(addr: &std::net::SocketAddr) -> bool {
    !addr.ip().is_loopback()
}

/// Advisory startup warning for a non-loopback bind, branched on
/// `TRUST_PROXY`: without it every visitor shares the proxy peer's quota;
/// with it the edge must overwrite (not append) the client-IP headers, or
/// clients pick their own identity. Either way TLS must terminate at the
/// edge. Warn-only — never fail boot for a listen address.
fn public_bind_warning(trust_proxy: bool) -> &'static str {
    if trust_proxy {
        "listening on a non-loopback address with TRUST_PROXY: terminate TLS at the edge (the __Host- admin cookie requires HTTPS) and make the edge overwrite X-Forwarded-For / X-Real-IP / Forwarded — appended client headers would let clients pick their own identity and quota"
    } else {
        "listening on a non-loopback address without proxy config: terminate TLS at the edge and note per-IP rate limits see the proxy peer, not the client"
    }
}

/// Run `PRAGMA quick_check` against the database and refuse to serve traffic
/// when it reports anything but a clean `ok`. Corruption must fail loud at
/// boot, not as silent row loss at request time.
fn check_db_quick(pool: &pool::SqlitePool) -> Result<(), String> {
    let conn = pool.get().map_err(|e| {
        format!(
            "database PRAGMA quick_check failed: cannot acquire a connection ({e}); \
             check DATABASE_PATH and file permissions"
        )
    })?;
    let mut stmt = conn
        .prepare("PRAGMA quick_check")
        .map_err(|e| format!("database PRAGMA quick_check failed: cannot prepare probe ({e})"))?;
    let rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("database PRAGMA quick_check failed: cannot run probe ({e})"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("database PRAGMA quick_check failed: cannot read probe rows ({e})"))?;
    if rows.len() == 1 && rows[0] == "ok" {
        return Ok(());
    }
    Err(format!(
        "database PRAGMA quick_check reported corruption: {}; \
         restore the SQLite file from backup (or re-import a known-good JSON export) before starting; \
         set DB_QUICK_CHECK=false only to bypass this gate for recovery",
        rows.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::{check_db_quick, is_public_bind, public_bind_warning};
    use zapiska::db::pool::{create_pool, run_migrations};

    #[test]
    fn quick_check_healthy_db_passes() {
        let dir = tempfile::tempdir().unwrap();
        let pool = create_pool(&dir.path().join("q.db").to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        assert!(check_db_quick(&pool).is_ok());
    }

    #[test]
    fn public_bind_predicate() {
        assert!(!is_public_bind(&"127.0.0.1:3000".parse().unwrap()));
        assert!(!is_public_bind(&"[::1]:3000".parse().unwrap()));
        assert!(is_public_bind(&"0.0.0.0:3000".parse().unwrap()));
        assert!(is_public_bind(&"[::]:3000".parse().unwrap()));
        assert!(is_public_bind(&"192.168.1.10:3000".parse().unwrap()));
    }

    #[test]
    fn public_bind_warning_branches_on_trust_proxy() {
        assert!(
            public_bind_warning(false).contains("without proxy config"),
            "unset branch keeps the peer-quota advisory"
        );
        let set = public_bind_warning(true);
        assert!(
            set.contains("TRUST_PROXY") && set.contains("overwrite"),
            "set branch must name the flag and the overwrite duty, got: {set}"
        );
    }

    #[test]
    fn quick_check_unreachable_db_fails_loud() {
        // A short connection timeout keeps the failure fast: r2d2 would
        // otherwise retry for its 30 s default before surfacing the error.
        let manager =
            r2d2_sqlite::SqliteConnectionManager::file("/nonexistent-dir-zapiska-quickcheck/q.db");
        let pool = r2d2::Pool::builder()
            .min_idle(Some(0))
            .connection_timeout(std::time::Duration::from_millis(200))
            .build(manager)
            .unwrap();
        let err = check_db_quick(&pool).unwrap_err();
        assert!(
            err.contains("quick_check"),
            "failure must name the check, got: {err}"
        );
    }
}
