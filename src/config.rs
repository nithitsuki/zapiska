use std::env;
use std::net::SocketAddr;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
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
    /// Telegram bot token for new-comment notifications (via Bot API).
    /// Requires `telegram_chat_id`; when both are set, every new comment
    /// posts a message to the chat. Optional — unset disables Telegram.
    pub telegram_bot_token: Option<String>,
    /// Telegram chat ID (numeric or @username) that receives notifications.
    pub telegram_chat_id: Option<String>,
    /// Override for the Telegram Bot API base URL. Defaults to the public
    /// endpoint. Useful for tests or for routing through a proxy.
    pub telegram_api_base: String,
    /// Slack Incoming Webhook URL for new-comment notifications.
    /// Optional — unset disables Slack.
    pub slack_webhook_url: Option<String>,
    /// Discord Incoming Webhook URL for new-comment notifications.
    /// Optional — unset disables Discord.
    pub discord_webhook_url: Option<String>,
    /// Notification batching window in seconds. New comments on the same page
    /// (or site-wide, see `notify_batch_granularity`) arriving inside the
    /// window are collected into a single digest message instead of one
    /// message per comment. `0` = send every comment immediately.
    pub notify_batch_secs: u64,
    /// Mid-window flush threshold: when the pending batch for a page reaches
    /// this count, it is flushed immediately with an aggregated message.
    /// `0` = window-based only (no threshold flush).
    pub notify_batch_threshold: u32,
    /// Batch window scoping: "page" = one window per target_path,
    /// "global" = a single site-wide window.
    pub notify_batch_granularity: String,
    /// Who may react to comments: "admin" (default — only requests with the
    /// admin token), or "anyone" (public, IP-hashed identity — HIGHLY
    /// discouraged without additional protections; reserved future value:
    /// "authenticated").
    pub reactions_allowed: String,
    /// Allowed reaction set, comma-separated (e.g. "👍,❤️,😄"). The API
    /// rejects anything outside this set.
    pub reactions_set: Vec<String>,
    /// Whitelist of comment languages as ISO 639-1 codes (e.g. "en,de,ja").
    /// Empty = no whitelist. When set, comments in other languages are
    /// rejected with 400. Wins over the blacklist when both are set.
    pub comment_lang_allowed: Vec<String>,
    /// Blacklist of comment languages as ISO 639-1 codes. Empty = no
    /// blacklist. Ignored when a whitelist is set.
    pub comment_lang_blocked: Vec<String>,
    /// Policy for undetectable / emoji-heavy comments: "always" (default,
    /// emoji-only comments pass), "never" (emoji-heavy rejected), or
    /// "if_unknown" (emoji-heavy pass, other undetectable text rejected).
    pub comment_lang_allow_emoji: String,
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
    #[error("NOTIFY_BATCH_GRANULARITY must be 'page' or 'global', got: {0}")]
    InvalidNotifyGranularity(String),
    #[error("PUBLIC_TARGET_ORIGIN must be an absolute http(s) URL, got: {0}")]
    InvalidPublicTargetOrigin(String),
    #[error("NOTIFY_BATCH_SECS must be a non-negative integer, got: {0}")]
    InvalidNotifyBatchSecs(String),
    #[error("NOTIFY_BATCH_THRESHOLD must be a non-negative integer, got: {0}")]
    InvalidNotifyBatchThreshold(String),
    #[error("REACTIONS_ALLOWED must be 'admin' or 'anyone', got: {0}")]
    InvalidReactionsMode(String),
    #[error("unknown language code '{0}' in COMMENT_LANG_ALLOWED/BLOCKED (use ISO 639-1)")]
    InvalidLangCode(String),
    #[error("COMMENT_LANG_ALLOW_EMOJI must be 'always', 'never', or 'if_unknown', got: {0}")]
    InvalidEmojiPolicy(String),
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

/// Redact a webhook URL to scheme + host + a truncated path prefix.
/// Webhook URLs embody bearer posting tokens, so the full URL must never
/// appear in startup logs.
fn redact_webhook_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("?");
            let path = parsed.path();
            let truncated = if path.len() > 14 {
                format!("{}…", &path[..14])
            } else {
                path.to_string()
            };
            format!("{}://{}{}", parsed.scheme(), host, truncated)
        }
        Err(_) => "***".to_string(),
    }
}

fn validate_public_target_origin(raw: &str) -> Result<String, ConfigError> {
    match url::Url::parse(raw) {
        Ok(parsed)
            if (parsed.scheme() == "http" || parsed.scheme() == "https")
                && parsed.host_str().is_some() =>
        {
            Ok(raw.to_string())
        }
        _ => Err(ConfigError::InvalidPublicTargetOrigin(raw.to_string())),
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind_addr: "127.0.0.1:3000".parse().expect("default bind_addr valid"),
            public_target_origin: "https://nithitsuki.com".to_string(),
            allowed_cors_origin: "https://nithitsuki.com".to_string(),
            admin_token: String::new(),
            database_path: "./comments.db".to_string(),
            github_token: None,
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
            turnstile_enabled: false,
            turnstile_secret_key: None,
            turnstile_verify_url: crate::turnstile::default_verify_url().to_string(),
            rate_limit_native_burst: 100,
            rate_limit_native_window_secs: 60,
            rate_limit_webmention_burst: 60,
            rate_limit_webmention_window_secs: 60,
            rate_limit_read_burst: 300,
            rate_limit_read_window_secs: 60,
            rate_limit_admin_moderate_burst: 30,
            rate_limit_admin_moderate_window_secs: 60,
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_api_base: "https://api.telegram.org".to_string(),
            slack_webhook_url: None,
            discord_webhook_url: None,
            notify_batch_secs: 60,
            notify_batch_threshold: 20,
            notify_batch_granularity: "page".to_string(),
            reactions_allowed: "admin".to_string(),
            reactions_set: vec![
                "👍".to_string(),
                "❤️".to_string(),
                "😄".to_string(),
                "😮".to_string(),
                "😢".to_string(),
                "😡".to_string(),
            ],
            comment_lang_allowed: Vec::new(),
            comment_lang_blocked: Vec::new(),
            comment_lang_allow_emoji: "always".to_string(),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let defaults = Config::default();
        let admin_token = env::var("ADMIN_TOKEN").map_err(|_| ConfigError::MissingAdminToken)?;

        let bind_addr_str = env_or_default("BIND_ADDR", &defaults.bind_addr.to_string());
        let bind_addr = SocketAddr::from_str(&bind_addr_str)
            .map_err(|_| ConfigError::InvalidBindAddr(bind_addr_str))?;

        let public_target_origin = validate_public_target_origin(&env_or_default(
            "PUBLIC_TARGET_ORIGIN",
            &defaults.public_target_origin,
        ))?;

        let allowed_cors_origin =
            env_or_default("ALLOWED_CORS_ORIGIN", &defaults.allowed_cors_origin);
        for part in allowed_cors_origin.split(',') {
            let part = part.trim();
            if part != "*" && !part.starts_with("http://") && !part.starts_with("https://") {
                return Err(ConfigError::CorsOriginInvalid(allowed_cors_origin));
            }
        }

        let database_path = env_or_default("DATABASE_PATH", &defaults.database_path);

        let github_token = match env::var("GITHUB_TOKEN") {
            Ok(s) if !s.is_empty() => Some(s),
            _ => None,
        };

        let max_content_len_raw =
            env_or_default("MAX_CONTENT_LEN", &defaults.max_content_len.to_string());
        let max_content_len = parse_or_err(
            "MAX_CONTENT_LEN",
            max_content_len_raw,
            ConfigError::InvalidContentLen,
        )?;
        if max_content_len == 0 {
            return Err(ConfigError::InvalidContentLen("0".to_string()));
        }

        let max_author_len_raw =
            env_or_default("MAX_AUTHOR_LEN", &defaults.max_author_len.to_string());
        let max_author_len = parse_or_err(
            "MAX_AUTHOR_LEN",
            max_author_len_raw,
            ConfigError::InvalidAuthorLen,
        )?;
        if max_author_len == 0 {
            return Err(ConfigError::InvalidAuthorLen("0".to_string()));
        }

        let max_body_size_raw =
            env_or_default("MAX_BODY_SIZE", &defaults.max_body_size.to_string());
        let max_body_size = parse_or_err(
            "MAX_BODY_SIZE",
            max_body_size_raw,
            ConfigError::InvalidBodySize,
        )?;
        if max_body_size == 0 {
            return Err(ConfigError::InvalidBodySize("0".to_string()));
        }

        let fetch_timeout_ms_raw =
            env_or_default("FETCH_TIMEOUT_MS", &defaults.fetch_timeout_ms.to_string());
        let fetch_timeout_ms = parse_or_err(
            "FETCH_TIMEOUT_MS",
            fetch_timeout_ms_raw,
            ConfigError::InvalidFetchTimeout,
        )?;
        if fetch_timeout_ms == 0 {
            return Err(ConfigError::InvalidFetchTimeout("0".to_string()));
        }

        let worker_backlog_raw =
            env_or_default("WORKER_BACKLOG", &defaults.worker_backlog.to_string());
        let worker_backlog = parse_or_err(
            "WORKER_BACKLOG",
            worker_backlog_raw,
            ConfigError::InvalidWorkerBacklog,
        )?;
        if worker_backlog == 0 {
            return Err(ConfigError::InvalidWorkerBacklog("0".to_string()));
        }

        let rust_log = env_or_default("RUST_LOG", &defaults.rust_log);

        let honeypot_field = env_or_default("HONEYPOT_FIELD", &defaults.honeypot_field);
        let max_comments_per_ip_per_day = env_or_default(
            "MAX_COMMENTS_PER_IP_PER_DAY",
            &defaults.max_comments_per_ip_per_day.to_string(),
        )
        .parse::<u32>()
        .unwrap_or(defaults.max_comments_per_ip_per_day);
        let max_webmentions_per_domain_per_hour = env_or_default(
            "MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR",
            &defaults.max_webmentions_per_domain_per_hour.to_string(),
        )
        .parse::<u32>()
        .unwrap_or(defaults.max_webmentions_per_domain_per_hour);

        let store_ip_address = env_bool("STORE_IP_ADDRESS", defaults.store_ip_address);
        let ip_hash_secret = env::var("IP_HASH_SECRET").ok().filter(|s| !s.is_empty());

        let moderation_webhook_url = env::var("MODERATION_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let moderation_webhook_mode =
            env_or_default("MODERATION_WEBHOOK_MODE", &defaults.moderation_webhook_mode);
        let default_comment_status =
            env_or_default("DEFAULT_COMMENT_STATUS", &defaults.default_comment_status);
        let max_thread_depth =
            env_or_default("MAX_THREAD_DEPTH", &defaults.max_thread_depth.to_string())
                .parse::<i64>()
                .unwrap_or(defaults.max_thread_depth)
                .clamp(0, 10);

        let turnstile_enabled = env_bool("TURNSTILE_ENABLED", defaults.turnstile_enabled);
        let turnstile_secret_key = env::var("TURNSTILE_SECRET_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        if turnstile_enabled && turnstile_secret_key.is_none() {
            return Err(ConfigError::TurnstileMissingSecret);
        }
        let turnstile_verify_url =
            env_or_default("TURNSTILE_VERIFY_URL", &defaults.turnstile_verify_url);
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

        let rate_limit_native_burst =
            rate_limit_burst("RATE_LIMIT_NATIVE", defaults.rate_limit_native_burst)?;
        let rate_limit_native_window_secs = rate_limit_window(
            "RATE_LIMIT_NATIVE_WINDOW",
            defaults.rate_limit_native_window_secs,
        )?;
        let rate_limit_webmention_burst = rate_limit_burst(
            "RATE_LIMIT_WEBMENTION",
            defaults.rate_limit_webmention_burst,
        )?;
        let rate_limit_webmention_window_secs = rate_limit_window(
            "RATE_LIMIT_WEBMENTION_WINDOW",
            defaults.rate_limit_webmention_window_secs,
        )?;
        let rate_limit_read_burst =
            rate_limit_burst("RATE_LIMIT_READ", defaults.rate_limit_read_burst)?;
        let rate_limit_read_window_secs = rate_limit_window(
            "RATE_LIMIT_READ_WINDOW",
            defaults.rate_limit_read_window_secs,
        )?;
        let rate_limit_admin_moderate_burst = rate_limit_burst(
            "RATE_LIMIT_ADMIN_MODERATE",
            defaults.rate_limit_admin_moderate_burst,
        )?;
        let rate_limit_admin_moderate_window_secs = rate_limit_window(
            "RATE_LIMIT_ADMIN_MODERATE_WINDOW",
            defaults.rate_limit_admin_moderate_window_secs,
        )?;

        let telegram_bot_token = env::var("TELEGRAM_BOT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let telegram_chat_id = env::var("TELEGRAM_CHAT_ID").ok().filter(|s| !s.is_empty());
        let telegram_api_base = env_or_default("TELEGRAM_API_BASE", &defaults.telegram_api_base);
        let slack_webhook_url = env::var("SLACK_WEBHOOK_URL").ok().filter(|s| !s.is_empty());
        let discord_webhook_url = env::var("DISCORD_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.is_empty());

        let notify_batch_secs = parse_or_err(
            "NOTIFY_BATCH_SECS",
            env_or_default("NOTIFY_BATCH_SECS", &defaults.notify_batch_secs.to_string()),
            ConfigError::InvalidNotifyBatchSecs,
        )?;
        let notify_batch_threshold = parse_or_err(
            "NOTIFY_BATCH_THRESHOLD",
            env_or_default(
                "NOTIFY_BATCH_THRESHOLD",
                &defaults.notify_batch_threshold.to_string(),
            ),
            ConfigError::InvalidNotifyBatchThreshold,
        )?;
        let notify_batch_granularity = env_or_default(
            "NOTIFY_BATCH_GRANULARITY",
            &defaults.notify_batch_granularity,
        )
        .to_lowercase();
        if notify_batch_granularity != "page" && notify_batch_granularity != "global" {
            return Err(ConfigError::InvalidNotifyGranularity(
                notify_batch_granularity,
            ));
        }

        let reactions_allowed =
            env_or_default("REACTIONS_ALLOWED", &defaults.reactions_allowed).to_lowercase();
        if reactions_allowed != "admin" && reactions_allowed != "anyone" {
            return Err(ConfigError::InvalidReactionsMode(reactions_allowed));
        }
        let reactions_set: Vec<String> =
            env_or_default("REACTIONS_SET", &defaults.reactions_set.join(","))
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

        fn parse_lang_codes(key: &str) -> Result<Vec<String>, ConfigError> {
            let raw = env::var(key).unwrap_or_default();
            let codes: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            for code in &codes {
                if crate::language::lang_from_iso_639_1(code).is_none() {
                    return Err(ConfigError::InvalidLangCode(code.clone()));
                }
            }
            Ok(codes)
        }

        let comment_lang_allowed = parse_lang_codes("COMMENT_LANG_ALLOWED")?;
        let comment_lang_blocked = parse_lang_codes("COMMENT_LANG_BLOCKED")?;
        let comment_lang_allow_emoji = env_or_default(
            "COMMENT_LANG_ALLOW_EMOJI",
            &defaults.comment_lang_allow_emoji,
        )
        .to_lowercase();
        if !matches!(
            comment_lang_allow_emoji.as_str(),
            "always" | "never" | "if_unknown"
        ) {
            return Err(ConfigError::InvalidEmojiPolicy(comment_lang_allow_emoji));
        }

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
            telegram_bot_token,
            telegram_chat_id,
            telegram_api_base,
            slack_webhook_url,
            discord_webhook_url,
            notify_batch_secs,
            notify_batch_threshold,
            notify_batch_granularity,
            reactions_allowed,
            reactions_set,
            comment_lang_allowed,
            comment_lang_blocked,
            comment_lang_allow_emoji,
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
                telegram_bot_token: {}, \
                telegram_chat_id: {}, \
                telegram_api_base: {}, \
                slack_webhook_url: {}, \
                discord_webhook_url: {}, \
                notify_batch_secs: {}, \
                notify_batch_threshold: {}, \
                notify_batch_granularity: {}, \
                reactions_allowed: {}, \
                reactions_set: {:?}, \
                comment_lang_allowed: {:?}, \
                comment_lang_blocked: {:?}, \
                comment_lang_allow_emoji: {}, \
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
                .map(redact_webhook_url)
                .unwrap_or("(unset)".to_string()),
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
            if self.0.telegram_bot_token.is_some() {
                "***"
            } else {
                "(unset)"
            },
            self.0.telegram_chat_id.as_deref().unwrap_or("(unset)"),
            self.0.telegram_api_base,
            self.0
                .slack_webhook_url
                .as_deref()
                .map(redact_webhook_url)
                .unwrap_or("(unset)".to_string()),
            self.0
                .discord_webhook_url
                .as_deref()
                .map(redact_webhook_url)
                .unwrap_or("(unset)".to_string()),
            self.0.notify_batch_secs,
            self.0.notify_batch_threshold,
            self.0.notify_batch_granularity,
            self.0.reactions_allowed,
            self.0.reactions_set,
            self.0.comment_lang_allowed,
            self.0.comment_lang_blocked,
            self.0.comment_lang_allow_emoji,
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

    /// Every environment variable `from_env` reads.
    const ENV_VARS: &[&str] = &[
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
        "TELEGRAM_BOT_TOKEN",
        "TELEGRAM_CHAT_ID",
        "TELEGRAM_API_BASE",
        "SLACK_WEBHOOK_URL",
        "DISCORD_WEBHOOK_URL",
        "NOTIFY_BATCH_SECS",
        "NOTIFY_BATCH_THRESHOLD",
        "NOTIFY_BATCH_GRANULARITY",
        "REACTIONS_ALLOWED",
        "REACTIONS_SET",
        "COMMENT_LANG_ALLOWED",
        "COMMENT_LANG_BLOCKED",
        "COMMENT_LANG_ALLOW_EMOJI",
    ];

    struct EnvCleaner {
        saved: Vec<(String, Option<String>)>,
    }

    impl Drop for EnvCleaner {
        fn drop(&mut self) {
            for (var, val) in self.saved.drain(..) {
                // SAFETY: held ENV_LOCK prevents concurrent env mutation.
                unsafe {
                    match val {
                        Some(v) => env::set_var(&var, v),
                        None => env::remove_var(&var),
                    }
                }
            }
        }
    }

    fn with_env(vars: &[(&str, &str)], f: impl FnOnce()) {
        let _lock = ENV_LOCK.lock().unwrap();
        with_env_locked(vars, f);
    }

    /// Like [`with_env`], but assumes `ENV_LOCK` is already held (for tests
    /// that must set ambient state atomically with the hermetic window).
    fn with_env_locked(vars: &[(&str, &str)], f: impl FnOnce()) {
        // Snapshot then clear every managed var so ambient environment
        // (e.g. RUST_LOG=debug exported in CI) cannot leak into `from_env`.
        let mut saved = Vec::with_capacity(ENV_VARS.len());
        for var in ENV_VARS {
            saved.push((var.to_string(), env::var(var).ok()));
            // SAFETY: held ENV_LOCK prevents concurrent env mutation.
            unsafe { env::remove_var(var) };
        }
        for (k, v) in vars {
            // SAFETY: held ENV_LOCK prevents concurrent env mutation.
            unsafe { env::set_var(k, v) };
        }
        // On drop, restores the snapshotted ambient values (instead of
        // permanently unsetting them, as the old cleaner did).
        let _cleaner = EnvCleaner { saved };
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
    fn rate_limit_defaults() {
        with_env(&[("ADMIN_TOKEN", "test-token")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.rate_limit_native_burst, 100);
            assert_eq!(config.rate_limit_native_window_secs, 60);
            assert_eq!(config.rate_limit_webmention_burst, 60);
            assert_eq!(config.rate_limit_webmention_window_secs, 60);
            assert_eq!(config.rate_limit_read_burst, 300);
            assert_eq!(config.rate_limit_read_window_secs, 60);
            assert_eq!(config.rate_limit_admin_moderate_burst, 30);
            assert_eq!(config.rate_limit_admin_moderate_window_secs, 60);
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
    fn empty_reactions_set_disables_reactions() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("REACTIONS_SET", " , , ")],
            || {
                let config = Config::from_env().unwrap();
                assert!(
                    config.reactions_set.is_empty(),
                    "empty set means reactions are disabled entirely"
                );
            },
        );
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
    fn notifications_disabled_by_default() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert!(config.telegram_bot_token.is_none());
            assert!(config.telegram_chat_id.is_none());
            assert_eq!(config.telegram_api_base, "https://api.telegram.org");
            assert!(config.slack_webhook_url.is_none());
        });
    }

    #[test]
    fn notifications_loaded_from_env() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("TELEGRAM_BOT_TOKEN", "123456:secret"),
                ("TELEGRAM_CHAT_ID", "@alerts"),
                ("TELEGRAM_API_BASE", "https://telegram-proxy.example"),
                (
                    "SLACK_WEBHOOK_URL",
                    "https://hooks.slack.com/services/x/y/z",
                ),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.telegram_bot_token.as_deref(), Some("123456:secret"));
                assert_eq!(config.telegram_chat_id.as_deref(), Some("@alerts"));
                assert_eq!(config.telegram_api_base, "https://telegram-proxy.example");
                assert_eq!(
                    config.slack_webhook_url.as_deref(),
                    Some("https://hooks.slack.com/services/x/y/z")
                );
            },
        );
    }

    #[test]
    fn empty_notification_vars_treated_as_none() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("TELEGRAM_BOT_TOKEN", ""),
                ("SLACK_WEBHOOK_URL", ""),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert!(config.telegram_bot_token.is_none());
                assert!(config.slack_webhook_url.is_none());
            },
        );
    }

    #[test]
    fn batch_settings_defaults() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.notify_batch_secs, 60);
            assert_eq!(config.notify_batch_threshold, 20);
            assert_eq!(config.notify_batch_granularity, "page");
            assert!(config.discord_webhook_url.is_none());
        });
    }

    #[test]
    fn batch_settings_loaded_from_env() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("NOTIFY_BATCH_SECS", "300"),
                ("NOTIFY_BATCH_THRESHOLD", "50"),
                ("NOTIFY_BATCH_GRANULARITY", "global"),
                (
                    "DISCORD_WEBHOOK_URL",
                    "https://discord.com/api/webhooks/1/abc",
                ),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.notify_batch_secs, 300);
                assert_eq!(config.notify_batch_threshold, 50);
                assert_eq!(config.notify_batch_granularity, "global");
                assert_eq!(
                    config.discord_webhook_url.as_deref(),
                    Some("https://discord.com/api/webhooks/1/abc")
                );
            },
        );
    }

    #[test]
    fn invalid_batch_granularity_rejected() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("NOTIFY_BATCH_GRANULARITY", "per-comment"),
            ],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidNotifyGranularity(_)));
            },
        );
    }

    #[test]
    fn reactions_defaults() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(config.reactions_allowed, "admin");
            assert_eq!(
                config.reactions_set,
                vec!["👍", "❤️", "😄", "😮", "😢", "😡"]
            );
        });
    }

    #[test]
    fn reactions_loaded_from_env() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("REACTIONS_ALLOWED", "anyone"),
                ("REACTIONS_SET", "👍, 👎, 🚀"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.reactions_allowed, "anyone");
                assert_eq!(config.reactions_set, vec!["👍", "👎", "🚀"]);
            },
        );
    }

    #[test]
    fn invalid_reactions_mode_rejected() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("REACTIONS_ALLOWED", "everyone")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidReactionsMode(_)));
            },
        );
    }

    #[test]
    fn language_filter_defaults_off() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert!(config.comment_lang_allowed.is_empty());
            assert!(config.comment_lang_blocked.is_empty());
            assert_eq!(config.comment_lang_allow_emoji, "always");
        });
    }

    #[test]
    fn language_codes_loaded_from_env() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("COMMENT_LANG_ALLOWED", "en, DE,ja"),
                ("COMMENT_LANG_BLOCKED", "ru"),
                ("COMMENT_LANG_ALLOW_EMOJI", "if_unknown"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.comment_lang_allowed, vec!["en", "de", "ja"]);
                assert_eq!(config.comment_lang_blocked, vec!["ru"]);
                assert_eq!(config.comment_lang_allow_emoji, "if_unknown");
            },
        );
    }

    #[test]
    fn unknown_language_code_rejected() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("COMMENT_LANG_ALLOWED", "en,xx")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidLangCode(_)));
            },
        );
        with_env(
            &[("ADMIN_TOKEN", "test"), ("COMMENT_LANG_BLOCKED", "klingon")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidLangCode(_)));
            },
        );
    }

    #[test]
    fn invalid_emoji_policy_rejected() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("COMMENT_LANG_ALLOW_EMOJI", "sometimes"),
            ],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::InvalidEmojiPolicy(_)));
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
            telegram_bot_token: Some("123456:ABC-secret-token".to_string()),
            telegram_chat_id: Some("@zapiska_alerts".to_string()),
            telegram_api_base: "https://api.telegram.org".to_string(),
            slack_webhook_url: Some("https://hooks.slack.com/services/T0/BBB/xxx".to_string()),
            discord_webhook_url: Some("https://discord.com/api/webhooks/1/abc".to_string()),
            notify_batch_secs: 60,
            notify_batch_threshold: 20,
            notify_batch_granularity: "page".to_string(),
            reactions_allowed: "admin".to_string(),
            reactions_set: vec!["👍".to_string(), "❤️".to_string()],
            comment_lang_allowed: vec!["en".to_string()],
            comment_lang_blocked: Vec::new(),
            comment_lang_allow_emoji: "always".to_string(),
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
        assert!(
            !rendered.contains("123456:ABC-secret-token"),
            "telegram_bot_token leaked"
        );
        assert!(
            rendered.contains("telegram_bot_token: ***"),
            "telegram_bot_token not redacted"
        );
        assert!(
            rendered.contains("telegram_chat_id: @zapiska_alerts"),
            "telegram_chat_id visible"
        );
        assert!(
            rendered.contains("telegram_api_base: https://api.telegram.org"),
            "telegram_api_base visible"
        );
        assert!(
            !rendered.contains("https://hooks.slack.com/services/T0/BBB/xxx"),
            "slack_webhook_url leaked"
        );
        assert!(
            rendered.contains("hooks.slack.com"),
            "slack webhook host visible"
        );
        assert!(
            !rendered.contains("https://discord.com/api/webhooks/1/abc"),
            "discord_webhook_url leaked"
        );
        assert!(
            rendered.contains("discord.com"),
            "discord webhook host visible"
        );
        assert!(
            rendered.contains("notify_batch_secs: 60"),
            "batch secs visible"
        );
        assert!(
            rendered.contains("notify_batch_threshold: 20"),
            "batch threshold visible"
        );
        assert!(
            rendered.contains("notify_batch_granularity: page"),
            "batch granularity visible"
        );
        // sanity: normal fields are still visible
        assert!(rendered.contains("127.0.0.1:3000"));
    }

    #[test]
    fn public_target_origin_garbage_rejected() {
        for bad in [
            "htts://example.com",
            "not-a-url",
            "ftp://example.com",
            "/relative/path",
            "",
        ] {
            with_env(
                &[("ADMIN_TOKEN", "test"), ("PUBLIC_TARGET_ORIGIN", bad)],
                || {
                    let err = Config::from_env().unwrap_err();
                    assert!(
                        matches!(err, ConfigError::InvalidPublicTargetOrigin(_)),
                        "origin {bad:?} must fail with InvalidPublicTargetOrigin, got: {err}"
                    );
                },
            );
        }
    }

    #[test]
    fn public_target_origin_valid_accepted() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("PUBLIC_TARGET_ORIGIN", "https://example.com"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.public_target_origin, "https://example.com");
            },
        );
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("PUBLIC_TARGET_ORIGIN", "http://localhost:3000"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.public_target_origin, "http://localhost:3000");
            },
        );
    }

    #[test]
    fn store_ip_address_bool_parsing_unified() {
        for truthy in ["true", "TRUE", "True", "1"] {
            with_env(
                &[("ADMIN_TOKEN", "test"), ("STORE_IP_ADDRESS", truthy)],
                || {
                    let config = Config::from_env().unwrap();
                    assert!(
                        config.store_ip_address,
                        "STORE_IP_ADDRESS={truthy:?} must enable storage"
                    );
                },
            );
        }
        with_env(
            &[("ADMIN_TOKEN", "test"), ("STORE_IP_ADDRESS", "false")],
            || {
                let config = Config::from_env().unwrap();
                assert!(!config.store_ip_address);
            },
        );
    }

    #[test]
    fn notify_batch_garbage_names_right_variable() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("NOTIFY_BATCH_SECS", "garbage")],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(
                    matches!(err, ConfigError::InvalidNotifyBatchSecs(_)),
                    "NOTIFY_BATCH_SECS=garbage must fail with InvalidNotifyBatchSecs, got: {err}"
                );
                assert!(
                    err.to_string().contains("NOTIFY_BATCH_SECS"),
                    "error must name NOTIFY_BATCH_SECS, got: {err}"
                );
            },
        );
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("NOTIFY_BATCH_THRESHOLD", "garbage"),
            ],
            || {
                let err = Config::from_env().unwrap_err();
                assert!(
                    matches!(err, ConfigError::InvalidNotifyBatchThreshold(_)),
                    "NOTIFY_BATCH_THRESHOLD=garbage must fail with InvalidNotifyBatchThreshold, got: {err}"
                );
                assert!(
                    err.to_string().contains("NOTIFY_BATCH_THRESHOLD"),
                    "error must name NOTIFY_BATCH_THRESHOLD, got: {err}"
                );
            },
        );
    }

    #[test]
    fn redacted_display_hides_webhook_urls() {
        let config = Config {
            admin_token: "test".to_string(),
            slack_webhook_url: Some(
                "https://hooks.slack.com/services/T000/B000/secret-token-xyz".to_string(),
            ),
            discord_webhook_url: Some(
                "https://discord.com/api/webhooks/123/secret-token-abc".to_string(),
            ),
            moderation_webhook_url: Some(
                "https://mod.example.com/hook?token=secret-123".to_string(),
            ),
            ..Config::default()
        };
        let rendered = format!("{}", config.redacted_display());
        assert!(
            !rendered.contains("secret-token-xyz"),
            "slack webhook secret leaked"
        );
        assert!(
            !rendered.contains("secret-token-abc"),
            "discord webhook secret leaked"
        );
        assert!(
            !rendered.contains("secret-123"),
            "moderation webhook secret leaked"
        );
        assert!(
            rendered.contains("hooks.slack.com"),
            "slack host should stay for ops: {rendered}"
        );
        assert!(
            rendered.contains("discord.com"),
            "discord host should stay for ops: {rendered}"
        );
    }

    #[test]
    fn default_matches_from_env_with_empty_env() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let from_env = Config::from_env().unwrap();
            let expected = Config {
                admin_token: "test".to_string(),
                ..Config::default()
            };
            assert_eq!(from_env, expected);
        });
    }

    #[test]
    fn silent_default_fields_match_default_impl() {
        // These three fields keep the silent `unwrap_or` policy but must still
        // read their defaults from `Config::default()` — they were the last
        // hand-synced literals in `from_env`, so a drifted literal here would
        // silently diverge from `Default`. If any literal drifts, this fails.
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let from_env = Config::from_env().unwrap();
            let defaults = Config::default();
            assert_eq!(
                from_env.max_comments_per_ip_per_day,
                defaults.max_comments_per_ip_per_day
            );
            assert_eq!(
                from_env.max_webmentions_per_domain_per_hour,
                defaults.max_webmentions_per_domain_per_hour
            );
            assert_eq!(from_env.max_thread_depth, defaults.max_thread_depth);
        });
    }

    #[test]
    fn with_env_is_hermetic_against_ambient_vars() {
        // Simulate ambient pollution (e.g. CI exporting RUST_LOG=debug):
        // `with_env` must clear it for the duration and restore it after.
        // The whole sequence holds ENV_LOCK so no other env test can
        // interleave between the pollution and the hermetic window.
        let _lock = ENV_LOCK.lock().unwrap();
        let ambient_before = env::var("RUST_LOG").ok();
        // SAFETY: ENV_LOCK is held.
        unsafe { env::set_var("RUST_LOG", "polluted-by-ambient") };
        with_env_locked(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(
                config.rust_log, "info",
                "ambient RUST_LOG must not leak into from_env"
            );
        });
        assert_eq!(
            env::var("RUST_LOG").as_deref(),
            Ok("polluted-by-ambient"),
            "with_env must restore ambient vars afterwards"
        );
        // Restore the pre-existing state so we don't pollute other tests.
        // SAFETY: ENV_LOCK is held.
        unsafe {
            match ambient_before {
                Some(v) => env::set_var("RUST_LOG", v),
                None => env::remove_var("RUST_LOG"),
            }
        }
    }
}
