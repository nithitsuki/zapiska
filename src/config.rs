use std::env;
use std::net::SocketAddr;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub public_target_origin: String,
    pub allowed_cors_origin: String,
    pub admin_token: String,
    pub database_path: String,
    pub github_token: Option<String>,
    pub max_content_len: usize,
    pub max_author_len: usize,
    pub max_body_size: usize,
    pub fetch_timeout_ms: u64,
    pub worker_backlog: usize,
    pub rust_log: String,
    /// Name of the honeypot form field. When non-empty, the submission is stored
    /// with `honeypot = 1` (flagged for moderator review, not discarded).
    /// The field is hidden from human users via CSS.
    pub honeypot_field: String,
    /// Max native comments per IP per day (resets at midnight UTC). 0 = unlimited.
    pub max_comments_per_ip_per_day: u32,
    /// Max webmentions per source domain per hour. 0 = unlimited.
    pub max_webmentions_per_domain_per_hour: u32,
    /// Whether to store the submitter's IP address with each comment.
    /// Disabled by default for privacy. Set to "true" to enable IP-based
    /// spam analysis in moderation scripts.
    pub store_ip_address: bool,
    /// Secret salt used when hashing IP addresses with SHA-256.
    /// When set, the salt is mixed into the hash to prevent rainbow table
    /// attacks. Only used when `store_ip_address` is also enabled.
    pub ip_hash_secret: Option<String>,
    /// Optional URL of an external moderation webhook. When set, zapiska
    /// POSTs the full comment data to this URL after every submission.
    /// The external service can use the admin API for additional context
    /// and call `/api/admin/moderate` to make a decision at any time.
    pub moderation_webhook_url: Option<String>,
    /// Webhook mode: "async" (fire-and-forget, default) or "sync" (wait for response).
    pub moderation_webhook_mode: String,
    /// Default moderation status for new comments.
    /// `"pending"` = manual review required (default).
    /// `"approved"` = auto-approve (posts appear immediately).
    /// Either way, the moderation webhook is still notified if configured.
    pub default_comment_status: String,
    /// Maximum nesting depth for threaded replies. 0 = disabled (no nesting).
    pub max_thread_depth: i64,
    /// Whether Cloudflare Turnstile verification is required on native comment
    /// submissions. Off by default — when `false`, the `cf-turnstile-response`
    /// form field is ignored entirely.
    pub turnstile_enabled: bool,
    /// Cloudflare Turnstile secret key (the *secret*, never the public sitekey).
    /// Required when `turnstile_enabled = true`. Loaded once at startup and
    /// never written to disk or logs.
    pub turnstile_secret_key: Option<String>,
    /// Override for the siteverify endpoint. Defaults to the public Cloudflare
    /// endpoint. Useful for tests or for routing through a proxy.
    pub turnstile_verify_url: String,
    /// Rate-limit burst for native comment submission (/api/comment).
    pub rate_limit_native_burst: u32,
    /// Rate-limit window (seconds) for native comment submission.
    pub rate_limit_native_window_secs: u64,
    /// Rate-limit burst for webmention ingress (/api/webmention).
    pub rate_limit_webmention_burst: u32,
    /// Rate-limit window (seconds) for webmention ingress.
    pub rate_limit_webmention_window_secs: u64,
    /// Rate-limit burst for public read API (/api/comments).
    pub rate_limit_read_burst: u32,
    /// Rate-limit window (seconds) for public read API.
    pub rate_limit_read_window_secs: u64,
    /// Rate-limit burst for admin moderate (/api/admin/moderate).
    pub rate_limit_admin_moderate_burst: u32,
    /// Rate-limit window (seconds) for admin moderate.
    pub rate_limit_admin_moderate_window_secs: u64,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("ADMIN_TOKEN is required but not set")]
    MissingAdminToken,
    #[error("ALLOWED_CORS_ORIGIN must be an absolute http(s) URL or *, got: {0}")]
    CorsOriginInvalid(String),
    #[error("FETCH_TIMEOUT_MS must be a positive integer, got: {0}")]
    InvalidFetchTimeout(String),
    #[error("BIND_ADDR is not a valid SocketAddr, got: {0}")]
    InvalidBindAddr(String),
    #[error("MAX_CONTENT_LEN must be a positive integer, got: {0}")]
    InvalidContentLen(String),
    #[error("MAX_AUTHOR_LEN must be a positive integer, got: {0}")]
    InvalidAuthorLen(String),
    #[error("MAX_BODY_SIZE must be a positive integer, got: {0}")]
    InvalidBodySize(String),
    #[error("WORKER_BACKLOG must be a positive integer, got: {0}")]
    InvalidWorkerBacklog(String),
    #[error("TURNSTILE_ENABLED is true but TURNSTILE_SECRET_KEY is not set")]
    TurnstileMissingSecret,
    #[error("TURNSTILE_VERIFY_URL must be an absolute https URL, got: {0}")]
    InvalidTurnstileVerifyUrl(String),
    #[error("RATE_LIMIT_*_WINDOW must be a positive integer, got: {0}")]
    InvalidRateLimitWindow(String),
    #[error("RATE_LIMIT_*_BURST must be a positive integer, got: {0}")]
    InvalidRateLimitBurst(String),
}

fn env_or_default(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_bool(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(s) => s.eq_ignore_ascii_case("true") || s == "1",
        Err(_) => default,
    }
}

fn parse_or_err<T: FromStr>(
    _key: &str,
    val: String,
    err_variant: fn(String) -> ConfigError,
) -> Result<T, ConfigError> {
    val.parse::<T>().map_err(|_| err_variant(val))
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let admin_token = env::var("ADMIN_TOKEN").map_err(|_| ConfigError::MissingAdminToken)?;

        let bind_addr_str = env_or_default("BIND_ADDR", "127.0.0.1:3000");
        let bind_addr = SocketAddr::from_str(&bind_addr_str)
            .map_err(|_| ConfigError::InvalidBindAddr(bind_addr_str))?;

        let public_target_origin = env_or_default("PUBLIC_TARGET_ORIGIN", "https://nithitsuki.com");

        let allowed_cors_origin = env_or_default("ALLOWED_CORS_ORIGIN", "https://nithitsuki.com");
        for part in allowed_cors_origin.split(',') {
            let part = part.trim();
            if part != "*" && !part.starts_with("http://") && !part.starts_with("https://") {
                return Err(ConfigError::CorsOriginInvalid(allowed_cors_origin));
            }
        }

        let database_path = env_or_default("DATABASE_PATH", "./comments.db");

        let github_token = match env::var("GITHUB_TOKEN") {
            Ok(s) if !s.is_empty() => Some(s),
            _ => None,
        };

        let max_content_len_raw = env_or_default("MAX_CONTENT_LEN", "2000");
        let max_content_len = parse_or_err(
            "MAX_CONTENT_LEN",
            max_content_len_raw,
            ConfigError::InvalidContentLen,
        )?;
        if max_content_len == 0 {
            return Err(ConfigError::InvalidContentLen("0".to_string()));
        }

        let max_author_len_raw = env_or_default("MAX_AUTHOR_LEN", "100");
        let max_author_len = parse_or_err(
            "MAX_AUTHOR_LEN",
            max_author_len_raw,
            ConfigError::InvalidAuthorLen,
        )?;
        if max_author_len == 0 {
            return Err(ConfigError::InvalidAuthorLen("0".to_string()));
        }

        let max_body_size_raw = env_or_default("MAX_BODY_SIZE", "8192");
        let max_body_size = parse_or_err(
            "MAX_BODY_SIZE",
            max_body_size_raw,
            ConfigError::InvalidBodySize,
        )?;
        if max_body_size == 0 {
            return Err(ConfigError::InvalidBodySize("0".to_string()));
        }

        let fetch_timeout_ms_raw = env_or_default("FETCH_TIMEOUT_MS", "4000");
        let fetch_timeout_ms = parse_or_err(
            "FETCH_TIMEOUT_MS",
            fetch_timeout_ms_raw,
            ConfigError::InvalidFetchTimeout,
        )?;
        if fetch_timeout_ms == 0 {
            return Err(ConfigError::InvalidFetchTimeout("0".to_string()));
        }

        let worker_backlog_raw = env_or_default("WORKER_BACKLOG", "64");
        let worker_backlog = parse_or_err(
            "WORKER_BACKLOG",
            worker_backlog_raw,
            ConfigError::InvalidWorkerBacklog,
        )?;
        if worker_backlog == 0 {
            return Err(ConfigError::InvalidWorkerBacklog("0".to_string()));
        }

        let rust_log = env_or_default("RUST_LOG", "info");

        let honeypot_field = env_or_default("HONEYPOT_FIELD", "website");
        let max_comments_per_ip_per_day = env_or_default("MAX_COMMENTS_PER_IP_PER_DAY", "50")
            .parse::<u32>()
            .unwrap_or(50);
        let max_webmentions_per_domain_per_hour =
            env_or_default("MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR", "10")
                .parse::<u32>()
                .unwrap_or(10);

        let store_ip_address = env_or_default("STORE_IP_ADDRESS", "false") == "true";
        let ip_hash_secret = env::var("IP_HASH_SECRET").ok().filter(|s| !s.is_empty());

        let moderation_webhook_url = env::var("MODERATION_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let moderation_webhook_mode = env_or_default("MODERATION_WEBHOOK_MODE", "async");
        let default_comment_status = env_or_default("DEFAULT_COMMENT_STATUS", "pending");
        let max_thread_depth = env_or_default("MAX_THREAD_DEPTH", "0")
            .parse::<i64>()
            .unwrap_or(0)
            .clamp(0, 10);

        let turnstile_enabled = env_bool("TURNSTILE_ENABLED", false);
        let turnstile_secret_key = env::var("TURNSTILE_SECRET_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        if turnstile_enabled && turnstile_secret_key.is_none() {
            return Err(ConfigError::TurnstileMissingSecret);
        }
        let turnstile_verify_url = env_or_default(
            "TURNSTILE_VERIFY_URL",
            "https://challenges.cloudflare.com/turnstile/v0/siteverify",
        );
        if !turnstile_verify_url.starts_with("https://") {
            return Err(ConfigError::InvalidTurnstileVerifyUrl(turnstile_verify_url));
        }

        fn rate_limit_burst(key: &str, default: u32) -> Result<u32, ConfigError> {
            let raw = env_or_default(key, &default.to_string());
            let val = raw
                .parse::<u32>()
                .map_err(|_| ConfigError::InvalidRateLimitBurst(raw))?;
            if val == 0 {
                return Err(ConfigError::InvalidRateLimitBurst("0".to_string()));
            }
            Ok(val)
        }

        fn rate_limit_window(key: &str, default: u64) -> Result<u64, ConfigError> {
            let raw = env_or_default(key, &default.to_string());
            let val = raw
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidRateLimitWindow(raw))?;
            if val == 0 {
                return Err(ConfigError::InvalidRateLimitWindow("0".to_string()));
            }
            Ok(val)
        }

        let rate_limit_native_burst = rate_limit_burst("RATE_LIMIT_NATIVE", 50)?;
        let rate_limit_native_window_secs = rate_limit_window("RATE_LIMIT_NATIVE_WINDOW", 60)?;
        let rate_limit_webmention_burst = rate_limit_burst("RATE_LIMIT_WEBMENTION", 30)?;
        let rate_limit_webmention_window_secs =
            rate_limit_window("RATE_LIMIT_WEBMENTION_WINDOW", 60)?;
        let rate_limit_read_burst = rate_limit_burst("RATE_LIMIT_READ", 60)?;
        let rate_limit_read_window_secs = rate_limit_window("RATE_LIMIT_READ_WINDOW", 60)?;
        let rate_limit_admin_moderate_burst = rate_limit_burst("RATE_LIMIT_ADMIN_MODERATE", 10)?;
        let rate_limit_admin_moderate_window_secs =
            rate_limit_window("RATE_LIMIT_ADMIN_MODERATE_WINDOW", 60)?;

        Ok(Config {
            bind_addr,
            public_target_origin,
            allowed_cors_origin,
            admin_token,
            database_path,
            github_token,
            max_content_len,
            max_author_len,
            max_body_size,
            fetch_timeout_ms,
            worker_backlog,
            rust_log,
            honeypot_field,
            max_comments_per_ip_per_day,
            max_webmentions_per_domain_per_hour,
            store_ip_address,
            ip_hash_secret,
            moderation_webhook_url,
            moderation_webhook_mode,
            default_comment_status,
            max_thread_depth,
            turnstile_enabled,
            turnstile_secret_key,
            turnstile_verify_url,
            rate_limit_native_burst,
            rate_limit_native_window_secs,
            rate_limit_webmention_burst,
            rate_limit_webmention_window_secs,
            rate_limit_read_burst,
            rate_limit_read_window_secs,
            rate_limit_admin_moderate_burst,
            rate_limit_admin_moderate_window_secs,
        })
    }

    pub fn redacted_display(&self) -> RedactedConfig<'_> {
        RedactedConfig(self)
    }
}

pub struct RedactedConfig<'a>(&'a Config);

impl std::fmt::Display for RedactedConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gh = self
            .0
            .github_token
            .as_deref()
            .map(|_| "***")
            .unwrap_or("(unset)");
        write!(
            f,
            "Config {{ \
                bind_addr: {}, \
                public_target_origin: {}, \
                allowed_cors_origin: {}, \
                admin_token: ***, \
                database_path: {}, \
                github_token: {}, \
                max_content_len: {}, \
                max_author_len: {}, \
                max_body_size: {}, \
                fetch_timeout_ms: {}, \
                worker_backlog: {}, \
                honeypot_field: {}, \
                max_comments_per_ip_per_day: {}, \
                max_webmentions_per_domain_per_hour: {}, \
                store_ip_address: {}, \
                ip_hash_secret: {}, \
                moderation_webhook_url: {}, \
                moderation_webhook_mode: {}, \
                default_comment_status: {}, \
                max_thread_depth: {}, \
                turnstile_enabled: {}, \
                turnstile_secret_key: {}, \
                turnstile_verify_url: {}, \
                rate_limit_native_burst: {}, \
                rate_limit_native_window_secs: {}, \
                rate_limit_webmention_burst: {}, \
                rate_limit_webmention_window_secs: {}, \
                rate_limit_read_burst: {}, \
                rate_limit_read_window_secs: {}, \
                rate_limit_admin_moderate_burst: {}, \
                rate_limit_admin_moderate_window_secs: {}, \
                rust_log: {} \
            }}",
            self.0.bind_addr,
            self.0.public_target_origin,
            self.0.allowed_cors_origin,
            self.0.database_path,
            gh,
            self.0.max_content_len,
            self.0.max_author_len,
            self.0.max_body_size,
            self.0.fetch_timeout_ms,
            self.0.worker_backlog,
            self.0.honeypot_field,
            self.0.max_comments_per_ip_per_day,
            self.0.max_webmentions_per_domain_per_hour,
            self.0.store_ip_address,
            if self.0.ip_hash_secret.is_some() {
                "***"
            } else {
                "(unset)"
            },
            self.0
                .moderation_webhook_url
                .as_deref()
                .unwrap_or("(unset)"),
            self.0.moderation_webhook_mode,
            self.0.default_comment_status,
            self.0.max_thread_depth,
            self.0.turnstile_enabled,
            if self.0.turnstile_secret_key.is_some() {
                "***"
            } else {
                "(unset)"
            },
            self.0.turnstile_verify_url,
            self.0.rate_limit_native_burst,
            self.0.rate_limit_native_window_secs,
            self.0.rate_limit_webmention_burst,
            self.0.rate_limit_webmention_window_secs,
            self.0.rate_limit_read_burst,
            self.0.rate_limit_read_window_secs,
            self.0.rate_limit_admin_moderate_burst,
            self.0.rate_limit_admin_moderate_window_secs,
            self.0.rust_log,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises env-var-dependent tests so they don't race in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvCleaner;

    impl Drop for EnvCleaner {
        fn drop(&mut self) {
            let vars = [
                "ADMIN_TOKEN",
                "BIND_ADDR",
                "PUBLIC_TARGET_ORIGIN",
                "ALLOWED_CORS_ORIGIN",
                "DATABASE_PATH",
                "GITHUB_TOKEN",
                "MAX_CONTENT_LEN",
                "MAX_AUTHOR_LEN",
                "MAX_BODY_SIZE",
                "FETCH_TIMEOUT_MS",
                "WORKER_BACKLOG",
                "RUST_LOG",
                "HONEYPOT_FIELD",
                "MAX_COMMENTS_PER_IP_PER_DAY",
                "MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR",
                "STORE_IP_ADDRESS",
                "IP_HASH_SECRET",
                "MODERATION_WEBHOOK_URL",
                "MODERATION_WEBHOOK_MODE",
                "DEFAULT_COMMENT_STATUS",
                "MAX_THREAD_DEPTH",
                "TURNSTILE_ENABLED",
                "TURNSTILE_SECRET_KEY",
                "TURNSTILE_VERIFY_URL",
                "RATE_LIMIT_NATIVE",
                "RATE_LIMIT_NATIVE_WINDOW",
                "RATE_LIMIT_WEBMENTION",
                "RATE_LIMIT_WEBMENTION_WINDOW",
                "RATE_LIMIT_READ",
                "RATE_LIMIT_READ_WINDOW",
                "RATE_LIMIT_ADMIN_MODERATE",
                "RATE_LIMIT_ADMIN_MODERATE_WINDOW",
            ];
            for var in vars {
                // SAFETY: held ENV_LOCK prevents concurrent env mutation.
                unsafe { env::remove_var(var) };
            }
        }
    }

    fn with_env(vars: &[(&str, &str)], f: impl FnOnce()) {
        let _lock = ENV_LOCK.lock().unwrap();
        for (k, v) in vars {
            // SAFETY: held ENV_LOCK prevents concurrent env mutation.
            unsafe { env::set_var(k, v) };
        }
        let _cleaner = EnvCleaner;
        f();
    }

    #[test]
    fn missing_admin_token_panics() {
        with_env(&[], || {
            let err = Config::from_env().unwrap_err();
            assert!(matches!(err, ConfigError::MissingAdminToken));
        });
    }

    #[test]
    fn defaults_loaded() {
        with_env(&[("ADMIN_TOKEN", "test-token")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.bind_addr.to_string(), "127.0.0.1:3000");
            assert_eq!(config.public_target_origin, "https://nithitsuki.com");
            assert_eq!(config.allowed_cors_origin, "https://nithitsuki.com");
            assert_eq!(config.admin_token, "test-token");
            assert_eq!(config.database_path, "./comments.db");
            assert!(config.github_token.is_none());
            assert_eq!(config.max_content_len, 2000);
            assert_eq!(config.max_author_len, 100);
            assert_eq!(config.max_body_size, 8192);
            assert_eq!(config.fetch_timeout_ms, 4000);
            assert_eq!(config.worker_backlog, 64);
            assert_eq!(config.rust_log, "info");
        });
    }

    #[test]
    fn overrides_via_env() {
        with_env(
            &[
                ("ADMIN_TOKEN", "my-secret"),
                ("BIND_ADDR", "0.0.0.0:9090"),
                ("PUBLIC_TARGET_ORIGIN", "https://example.com"),
                ("ALLOWED_CORS_ORIGIN", "https://example.com"),
                ("DATABASE_PATH", "/data/comments.db"),
                ("GITHUB_TOKEN", "ghp_xxx"),
                ("MAX_CONTENT_LEN", "1000"),
                ("MAX_AUTHOR_LEN", "50"),
                ("MAX_BODY_SIZE", "4096"),
                ("FETCH_TIMEOUT_MS", "2000"),
                ("WORKER_BACKLOG", "128"),
                ("RUST_LOG", "debug"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.bind_addr.to_string(), "0.0.0.0:9090");
                assert_eq!(config.public_target_origin, "https://example.com");
                assert_eq!(config.allowed_cors_origin, "https://example.com");
                assert_eq!(config.admin_token, "my-secret");
                assert_eq!(config.database_path, "/data/comments.db");
                assert_eq!(config.github_token, Some("ghp_xxx".to_string()));
                assert_eq!(config.max_content_len, 1000);
                assert_eq!(config.max_author_len, 50);
                assert_eq!(config.max_body_size, 4096);
                assert_eq!(config.fetch_timeout_ms, 2000);
                assert_eq!(config.worker_backlog, 128);
                assert_eq!(config.rust_log, "debug");
            },
        );
    }

    #[test]
    fn empty_github_token_treated_as_none() {
        with_env(&[("ADMIN_TOKEN", "test"), ("GITHUB_TOKEN", "")], || {
            let config = Config::from_env().unwrap();
            assert!(config.github_token.is_none());
        });
    }

    #[test]
    fn cors_origin_allows_http_and_https_and_wildcard() {
        // http is valid (local dev)
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("ALLOWED_CORS_ORIGIN", "http://localhost:8000"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.allowed_cors_origin, "http://localhost:8000");
            },
        );
        // wildcard is valid
        with_env(
            &[("ADMIN_TOKEN", "test"), ("ALLOWED_CORS_ORIGIN", "*")],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.allowed_cors_origin, "*");
            },
        );
        // comma-separated origins are valid
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                (
                    "ALLOWED_CORS_ORIGIN",
                    "http://localhost:1313,https://nithitsuki.com",
                ),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(
                    config.allowed_cors_origin,
                    "http://localhost:1313,https://nithitsuki.com"
                );
            },
        );
        // ftp:// is rejected even in multi-origin
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("ALLOWED_CORS_ORIGIN", "ftp://evil"),
            ],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::CorsOriginInvalid(_)));
            },
        );
    }

    #[test]
    fn zero_fetch_timeout_rejected() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("FETCH_TIMEOUT_MS", "0")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidFetchTimeout(_)));
            },
        );
    }

    #[test]
    fn non_positive_max_content_len_rejected() {
        with_env(&[("ADMIN_TOKEN", "test"), ("MAX_CONTENT_LEN", "0")], || {
            let err = Config::from_env().unwrap_err();
            assert!(matches!(err, ConfigError::InvalidContentLen(_)));
        });
    }

    #[test]
    fn non_positive_worker_backlog_rejected() {
        with_env(&[("ADMIN_TOKEN", "test"), ("WORKER_BACKLOG", "0")], || {
            let err = Config::from_env().unwrap_err();
            assert!(matches!(err, ConfigError::InvalidWorkerBacklog(_)));
        });
    }

    #[test]
    fn invalid_bind_addr_rejected() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("BIND_ADDR", "not-a-socket")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidBindAddr(_)));
            },
        );
    }

    #[test]
    fn non_numeric_max_content_len_rejected() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("MAX_CONTENT_LEN", "abc")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidContentLen(_)));
            },
        );
    }

    #[test]
    fn turnstile_disabled_by_default() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert!(!config.turnstile_enabled);
            assert!(config.turnstile_secret_key.is_none());
            assert_eq!(
                config.turnstile_verify_url,
                "https://challenges.cloudflare.com/turnstile/v0/siteverify"
            );
        });
    }

    #[test]
    fn turnstile_enabled_requires_secret() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("TURNSTILE_ENABLED", "true")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::TurnstileMissingSecret));
            },
        );
    }

    #[test]
    fn turnstile_enabled_with_secret_loads() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("TURNSTILE_ENABLED", "true"),
                ("TURNSTILE_SECRET_KEY", "0x4AAAAAAAsecret"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert!(config.turnstile_enabled);
                assert_eq!(
                    config.turnstile_secret_key.as_deref(),
                    Some("0x4AAAAAAAsecret")
                );
            },
        );
    }

    #[test]
    fn turnstile_verify_url_must_be_https() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("TURNSTILE_ENABLED", "true"),
                ("TURNSTILE_SECRET_KEY", "k"),
                ("TURNSTILE_VERIFY_URL", "http://insecure.example/verify"),
            ],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidTurnstileVerifyUrl(_)));
            },
        );
    }

    #[test]
    fn redacted_display_hides_admin_and_github_tokens() {
        let config = Config {
            bind_addr: "127.0.0.1:3000".parse().unwrap(),
            public_target_origin: "https://nithitsuki.com".to_string(),
            allowed_cors_origin: "https://nithitsuki.com".to_string(),
            admin_token: "super-secret-12345".to_string(),
            database_path: "./comments.db".to_string(),
            github_token: Some("ghp_xxx".to_string()),
            max_content_len: 2000,
            max_author_len: 100,
            max_body_size: 8192,
            fetch_timeout_ms: 4000,
            worker_backlog: 64,
            rust_log: "info".to_string(),
            honeypot_field: "website".to_string(),
            max_comments_per_ip_per_day: 50,
            max_webmentions_per_domain_per_hour: 10,
            store_ip_address: false,
            ip_hash_secret: None,
            moderation_webhook_url: None,
            moderation_webhook_mode: "async".to_string(),
            default_comment_status: "pending".to_string(),
            max_thread_depth: 0,
            turnstile_enabled: true,
            turnstile_secret_key: Some("0x4AAAAAAAsecret".to_string()),
            turnstile_verify_url: "https://challenges.cloudflare.com/turnstile/v0/siteverify"
                .to_string(),
            rate_limit_native_burst: 50,
            rate_limit_native_window_secs: 60,
            rate_limit_webmention_burst: 30,
            rate_limit_webmention_window_secs: 60,
            rate_limit_read_burst: 60,
            rate_limit_read_window_secs: 60,
            rate_limit_admin_moderate_burst: 10,
            rate_limit_admin_moderate_window_secs: 60,
        };
        let rendered = format!("{}", config.redacted_display());
        assert!(
            !rendered.contains("super-secret-12345"),
            "admin_token leaked"
        );
        assert!(
            rendered.contains("admin_token: ***"),
            "admin_token not redacted"
        );
        assert!(!rendered.contains("ghp_xxx"), "github_token leaked");
        assert!(
            rendered.contains("github_token: ***"),
            "github_token not redacted"
        );
        assert!(
            !rendered.contains("0x4AAAAAAAsecret"),
            "turnstile_secret_key leaked"
        );
        assert!(
            rendered.contains("turnstile_secret_key: ***"),
            "turnstile_secret_key not redacted"
        );
        // sanity: normal fields are still visible
        assert!(rendered.contains("127.0.0.1:3000"));
    }
}
