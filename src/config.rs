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
    /// Optional URL of an external moderation webhook. When set, zapiska
    /// POSTs the full comment data to this URL after every submission.
    /// The external service can use the admin API for additional context
    /// and call `/api/admin/moderate` to make a decision at any time.
    pub moderation_webhook_url: Option<String>,
    /// Default moderation status for new comments.
    /// `"pending"` = manual review required (default).
    /// `"approved"` = auto-approve (posts appear immediately).
    /// Either way, the moderation webhook is still notified if configured.
    pub default_comment_status: String,
    /// Maximum nesting depth for threaded replies. 0 = disabled (no nesting).
    pub max_thread_depth: i64,
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
}

fn env_or_default(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
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
        if allowed_cors_origin != "*"
            && !allowed_cors_origin.starts_with("http://")
            && !allowed_cors_origin.starts_with("https://")
        {
            return Err(ConfigError::CorsOriginInvalid(allowed_cors_origin));
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

        let moderation_webhook_url = env::var("MODERATION_WEBHOOK_URL").ok().filter(|s| !s.is_empty());
        let default_comment_status = env_or_default("DEFAULT_COMMENT_STATUS", "pending");
        let max_thread_depth = env_or_default("MAX_THREAD_DEPTH", "0")
            .parse::<i64>()
            .unwrap_or(0)
            .clamp(0, 10);

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
            moderation_webhook_url,
            default_comment_status,
            max_thread_depth,
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
                moderation_webhook_url: {}, \
                default_comment_status: {}, \
                max_thread_depth: {}, \
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
            self.0.moderation_webhook_url.as_deref().unwrap_or("(unset)"),
            self.0.default_comment_status,
            self.0.max_thread_depth,
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
        // ftp:// is rejected
        with_env(
            &[("ADMIN_TOKEN", "test"), ("ALLOWED_CORS_ORIGIN", "ftp://evil")],
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
        moderation_webhook_url: None,
        default_comment_status: "pending".to_string(),
        max_thread_depth: 0,
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
        // sanity: normal fields are still visible
        assert!(rendered.contains("127.0.0.1:3000"));
    }
}
