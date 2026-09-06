use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use reqwest::Client;

use crate::config::Config;
use crate::db::pool::{self, SqlitePool};
use crate::db::repo::Repo;
use crate::github::{GitHubLookup, RealGitHub};
use crate::language::LanguageGate;
use crate::notify::NotificationBatcher;
#[cfg(feature = "webmentions")]
use crate::worker::JobSender;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub pool: SqlitePool,
    pub repo: Repo,
    pub github: Arc<dyn GitHubLookup>,
    /// Notification channels (Telegram / Slack / Discord) with batching.
    pub notifier: Arc<NotificationBatcher>,
    /// Optional language filtering for native comments (off by default).
    pub language: LanguageGate,
    #[cfg(feature = "webmentions")]
    pub wm_sender: JobSender,
    /// Shared HTTP client for operator-configured endpoints only (GitHub
    /// enrichment, moderation webhooks, notification delivery, Turnstile).
    /// Untrusted author/webmention URL fetches must go through SafeFetcher,
    /// never this client directly.
    pub http_client: Client,
    /// In-memory rate limiter for per-IP daily caps and per-domain hourly caps.
    pub limiter: Arc<Limiter>,
}

impl AppState {
    /// One assembly path for `main` + every test (T22): shared HTTP client,
    /// pool + migrations (a newer-than-binary schema fails loud here),
    /// corruption gate, repo, notifier, language gate, GitHub, the
    /// webmention worker (carrying the config's webhook sink, T20-F1), and
    /// the state itself. `Err(String)` carries the operator remedy;
    /// advisories (half-configured Telegram, public bind, lost secret) go
    /// to `tracing` like before. Tests needing a fake GitHub use
    /// [`AppState::start_with_github`]; everything else is identical.
    pub fn start(config: Config) -> Result<Self, String> {
        let (pool, repo, http_client, notifier, language) = Self::base(&config)?;
        let github: Arc<dyn GitHubLookup> = Arc::new(RealGitHub::new(
            repo.clone(),
            config.fetch_timeout_ms,
            config.github_token.clone(),
            http_client.clone(),
        ));
        Self::with_github(config, github, pool, repo, http_client, notifier, language)
    }

    /// Like [`AppState::start`] but with an injected [`GitHubLookup`]
    /// (tests pass `StubGitHub`). The only divergence from production
    /// assembly is the GitHub adapter.
    pub fn start_with_github(
        config: Config,
        github: Arc<dyn GitHubLookup>,
    ) -> Result<Self, String> {
        let (pool, repo, http_client, notifier, language) = Self::base(&config)?;
        Self::with_github(config, github, pool, repo, http_client, notifier, language)
    }

    /// Pool → migrations → corruption gate → repo → notifier → language.
    /// Shared by both entry points so a new field can only be wired once.
    fn base(
        config: &Config,
    ) -> Result<
        (
            SqlitePool,
            Repo,
            reqwest::Client,
            Arc<NotificationBatcher>,
            LanguageGate,
        ),
        String,
    > {
        // Shared HTTP client for operator-configured endpoints only (GitHub API,
        // moderation webhooks, notification channels, Turnstile siteverify).
        // Untrusted author/webmention URLs are fetched through SafeFetcher (its
        // own redirect-disabled client with per-hop SSRF checks), never this one.
        let http_client = reqwest::Client::builder()
            .user_agent(format!("webmention.nithitsuki.com/{}", crate::APP_VERSION))
            .timeout(std::time::Duration::from_millis(config.fetch_timeout_ms))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;

        let sqlite_pool = pool::create_pool(&config.database_path)
            .map_err(|e| format!("failed to create SQLite pool: {e}"))?;
        pool::run_migrations(&sqlite_pool, config.ip_hash_secret.as_deref())
            .map_err(|e| format!("failed to run migrations: {e}"))?;

        // Fail loud on corruption: a database that fails quick_check must not
        // serve traffic (and /healthz would report 503 for it anyway).
        if config.db_quick_check {
            pool::quick_check(&sqlite_pool)?;
        }

        if config.telegram_bot_token.is_some() != config.telegram_chat_id.is_some() {
            tracing::warn!(
                "Telegram notifications are half-configured (need both TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID); Telegram delivery is disabled"
            );
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

        let repo = Repo::new(sqlite_pool.clone());

        // A lost or rotated IP_HASH_SECRET silently splits IP-hash continuity:
        // stored hashes (comments, anyone-mode reaction identities) stop matching
        // freshly computed ones. Fail loud in the logs, not silent in the data.
        if ip_hash_secret_warning_needed(
            &config.ip_hash_secret,
            repo.count_salted_ip_hashes_sync().unwrap_or(0),
        ) {
            tracing::warn!(
                "IP_HASH_SECRET is unset but the database holds salted IP hashes; \
                 restore the original secret from your .env backup or hashes will mismatch"
            );
        }

        let notifier = Arc::new(NotificationBatcher::new(config));
        let language = LanguageGate::new(config);

        Ok((sqlite_pool, repo, http_client, notifier, language))
    }

    /// Finish assembly once the GitHub adapter is chosen: spawn the worker,
    /// then build the state.
    fn with_github(
        config: Config,
        github: Arc<dyn GitHubLookup>,
        sqlite_pool: SqlitePool,
        repo: Repo,
        http_client: reqwest::Client,
        notifier: Arc<NotificationBatcher>,
        language: LanguageGate,
    ) -> Result<Self, String> {
        // Webmention worker — only spawned when the feature is enabled.
        #[cfg(feature = "webmentions")]
        let (wm_sender, wm_receiver) = crate::worker::channel(config.worker_backlog);
        #[cfg(feature = "webmentions")]
        crate::worker::spawn_worker_for_state(
            wm_receiver,
            repo.clone(),
            http_client.clone(),
            Arc::clone(&github),
            config.public_target_origin.clone(),
            config.max_content_len,
            config.fetch_timeout_ms,
            notifier.clone(),
            config
                .worker_moderation_sink(&http_client)
                .map(|s| Arc::new(s) as Arc<dyn crate::moderation::ModerationSink>),
        );

        Ok(AppState {
            config,
            pool: sqlite_pool,
            repo,
            github,
            notifier,
            language,
            #[cfg(feature = "webmentions")]
            wm_sender,
            http_client,
            limiter: Arc::new(Limiter::new()),
        })
    }
}

/// True when the startup warning for a lost/rotated secret must fire: no
/// secret configured while the database already holds salted hashes.
fn ip_hash_secret_warning_needed(secret: &Option<String>, salted_rows: i64) -> bool {
    secret.is_none() && salted_rows > 0
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

/// Simple in-memory rate limiter keyed by `(prefix, period_key)`.
/// Period keys are date strings (YYYY-MM-DD) or hour strings (YYYY-MM-DD-HH).
/// Old period keys naturally expire — no one queries yesterday's date for today's limit.
/// The HashMap is cleaned up opportunistically when a new entry bumps into a stale key.
#[derive(Debug)]
pub struct Limiter {
    counts: Mutex<HashMap<String, u32>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Check and increment the count for a given key.
    /// Returns `true` if the operation is within the limit, `false` if it should be rejected.
    pub fn check_and_increment(&self, key: &str, limit: u32) -> bool {
        if limit == 0 {
            return true; // unlimited
        }
        let mut counts = self.counts.lock().expect("limiter lock");
        let count = counts.get(key).copied().unwrap_or(0);
        if count >= limit {
            return false;
        }
        counts.insert(key.to_string(), count + 1);

        // Opportunistic cleanup: if the map is over 10k entries, sweep stale keys.
        // A key is "stale" if its embedded date is older than yesterday.
        if counts.len() > 10_000 {
            let today = date_key();
            let yesterday = yesterday_key();
            counts.retain(|k, _| {
                // Keep keys that contain today's or yesterday's date prefix.
                // Keys look like "ip:2026-07-05" or "domain:2026-07-05-14".
                k.contains(&today) || k.contains(&yesterday)
            });
        }

        true
    }
}

/// Returns today's date as a key fragment: "2026-07-05".
fn date_key() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Days since epoch
    let days = secs / 86400;
    // Compute year/month/day from days since epoch (simple algorithm, good enough)
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn yesterday_key() -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let (y, m, d) = days_to_ymd(days.saturating_sub(1));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let (y, m, d) = crate::timeutil::ymd_from_days(days as i64);
    (y as u64, m as u64, d as u64)
}

/// Build a limiter key for per-IP-per-day tracking.
pub fn ip_daily_key(ip: &std::net::IpAddr) -> String {
    format!("ip:{ip}:{}", date_key())
}

/// Build a limiter key for per-domain-per-hour tracking.
pub fn domain_hourly_key(domain: &str) -> String {
    use std::time::SystemTime;
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let hours = dur.as_secs() / 3600;
    let days = hours / 24;
    let (y, m, d) = days_to_ymd(days);
    let h = hours % 24;
    format!("dom:{domain}:{y:04}-{m:02}-{d:02}-{h:02}")
}

#[cfg(test)]
mod start_tests {
    use super::*;
    use crate::github::StubGitHub;

    fn healthz_request() -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("GET")
            .uri("/healthz")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn start_boots_temp_db_to_healthz() {
        // T22 smoke: temp DB through the one assembly path serves healthz.
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            database_path: dir.path().join("smoke.db").to_string_lossy().to_string(),
            admin_token: "test".to_string(),
            ..Config::default()
        };
        let state = AppState::start_with_github(config, Arc::new(StubGitHub)).expect("start");
        let app = crate::http::build_app(state);
        let resp = tower::ServiceExt::oneshot(app, healthz_request())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "assembled state must serve healthz");
    }

    #[tokio::test]
    async fn start_refuses_newer_schema() {
        // T22: a database newer than the binary fails loud at assembly.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("future.db");
        let pool = pool::create_pool(&db_path.to_string_lossy()).expect("pool");
        pool::run_migrations(&pool, None).expect("migrations");
        pool.get()
            .expect("conn")
            .execute_batch(&format!(
                "PRAGMA user_version = {}",
                pool::LATEST_SCHEMA_VERSION + 1
            ))
            .expect("stamp future version");
        let config = Config {
            database_path: db_path.to_string_lossy().to_string(),
            admin_token: "test".to_string(),
            ..Config::default()
        };
        let err = match AppState::start_with_github(config, Arc::new(StubGitHub)) {
            Ok(_) => panic!("newer schema must refuse"),
            Err(err) => err,
        };
        assert!(
            err.contains("newer than supported"),
            "refusal must name the cause, got: {err}"
        );
    }

    #[test]
    fn secret_warning_predicate() {
        assert!(ip_hash_secret_warning_needed(&None, 1));
        assert!(ip_hash_secret_warning_needed(&None, 40));
        assert!(!ip_hash_secret_warning_needed(&None, 0));
        assert!(!ip_hash_secret_warning_needed(&Some("s".to_string()), 5));
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
}
