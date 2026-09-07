use std::env;
use std::net::SocketAddr;
use std::str::FromStr;
use thiserror::Error;

/// T21 subgroups: Server | Fetch | Moderation | RateLimit | Notify.
/// Fields stay flat (no renames) and are grouped by section comments below;
/// enums are parsed once at load so consumers never re-derive them.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    // ── Server ──
    pub bind_addr: SocketAddr,
    pub public_target_origin: url::Url,
    pub allowed_cors_origin: String,
    pub admin_token: String,
    pub database_path: String,
    /// Run `PRAGMA quick_check` at startup and refuse to serve traffic when
    /// the database reports corruption. On by default (quick_check does most
    /// of the checking of `integrity_check` but runs much faster, so boot
    /// latency is small); set `DB_QUICK_CHECK=false` to skip.
    pub db_quick_check: bool,
    /// Whether the server sits behind a trusted reverse proxy that sets
    /// client-IP headers. When `false` (default), `X-Forwarded-For`,
    /// `X-Real-IP`, and `Forwarded` are ignored and rate limits, quotas, and
    /// IP hashes key on the TCP peer address (spoof-proof). Set to `true`
    /// only when every byte arrives via a proxy you control — then
    /// `CF-Connecting-IP` (when present and valid) identifies the client,
    /// else the leftmost `X-Forwarded-For` entry (else `X-Real-IP`, else
    /// `Forwarded for=`). See `src/http/peer.rs`.
    pub trust_proxy: bool,
    // ── Fetch ──
    pub github_token: Option<String>,
    pub max_content_len: usize,
    pub max_author_len: usize,
    pub max_body_size: usize,
    pub fetch_timeout_ms: u64,
    pub worker_backlog: usize,
    // ── Moderation ──
    /// Name of the honeypot form field. When non-empty, the submission is stored
    /// with `honeypot = 1` (flagged for moderator review, not discarded).
    /// The field is hidden from human users via CSS.
    pub honeypot_field: String,
    /// Optional URL of an external moderation webhook. When set, zapiska
    /// POSTs the full comment data to this URL after every submission.
    /// The external service can use the admin API for additional context
    /// and call `/api/admin/moderate` to make a decision at any time.
    pub moderation_webhook_url: Option<String>,
    /// Webhook mode: `Async` (fire-and-forget, default) or `Sync` (wait for response).
    pub moderation_webhook_mode: WebhookMode,
    /// Optional HMAC secret signing every outbound moderation-webhook body
    /// (both async and sync emissions). Empty/unset = unsigned (backwards
    /// compatible). When set, each POST carries `X-Zapiska-Timestamp` and
    /// `X-Zapiska-Signature` headers (see `src/http/webhook.rs`); a consumer
    /// holding the secret rejects unsigned or tampered bodies. Additive only.
    pub webhook_signing_secret: Option<String>,
    /// Default moderation status for new comments.
    /// `Pending` = manual review required (default).
    /// `Approved` = auto-approve (posts appear immediately).
    /// Either way, the moderation webhook is still notified if configured.
    pub default_comment_status: crate::moderation::Status,
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
    /// Whether to store the submitter's IP address with each comment.
    /// Disabled by default for privacy. Set to "true" to enable IP-based
    /// spam analysis in moderation scripts.
    pub store_ip_address: bool,
    /// Secret salt used when hashing IP addresses with SHA-256.
    /// When set, the salt is mixed into the hash to prevent rainbow table
    /// attacks. Only used when `store_ip_address` is also enabled.
    pub ip_hash_secret: Option<String>,
    // ── RateLimit ──
    /// Max native comments per IP per day (resets at midnight UTC). 0 = unlimited.
    pub max_comments_per_ip_per_day: u32,
    /// Max webmentions per source domain per hour. 0 = unlimited.
    pub max_webmentions_per_domain_per_hour: u32,
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
    // ── Notify ──
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
    /// Batch window scoping: `Page` = one window per target_path,
    /// `Global` = a single site-wide window.
    pub notify_batch_granularity: BatchGranularity,
    /// Who may react to comments: `Admin` (default — only requests with the
    /// admin token), or `Anyone` (public, IP-hashed identity — HIGHLY
    /// discouraged without additional protections; reserved future value:
    /// "authenticated").
    pub reactions_allowed: ReactionsMode,
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
    /// Policy for undetectable / emoji-heavy comments: `Always` (default,
    /// emoji-only comments pass), `Never` (emoji-heavy rejected), or
    /// `IfUnknown` (emoji-heavy pass, other undetectable text rejected).
    pub comment_lang_allow_emoji: EmojiPolicy,
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
    #[error("HONEYPOT_FIELD must be 1-64 chars of [A-Za-z0-9_-], got: {0}")]
    InvalidHoneypotField(String),
    #[error("MODERATION_WEBHOOK_MODE must be 'async' or 'sync', got: {0}")]
    InvalidModerationWebhookMode(String),
    #[error("DEFAULT_COMMENT_STATUS must be 'pending' or 'approved', got: {0}")]
    InvalidDefaultStatus(String),
    #[error("MAX_COMMENTS_PER_IP_PER_DAY must be a non-negative integer, got: {0}")]
    InvalidMaxCommentsPerIpPerDay(String),
    #[error("MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR must be a non-negative integer, got: {0}")]
    InvalidMaxWebmentionsPerDomainPerHour(String),
    #[error("MAX_THREAD_DEPTH must be an integer, got: {0}")]
    InvalidMaxThreadDepth(String),
}

// ── Typed subgroups (parsed once at load; consumers never re-derive) ──

/// Batch window scoping for notification digests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchGranularity {
    Page,
    Global,
}

impl BatchGranularity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Global => "global",
        }
    }
}

impl std::fmt::Display for BatchGranularity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BatchGranularity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "page" => Ok(Self::Page),
            "global" => Ok(Self::Global),
            other => Err(other.to_string()),
        }
    }
}

/// Moderation webhook delivery mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookMode {
    Async,
    Sync,
}

impl WebhookMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Async => "async",
            Self::Sync => "sync",
        }
    }

    #[must_use]
    pub fn is_sync(self) -> bool {
        matches!(self, Self::Sync)
    }
}

impl std::fmt::Display for WebhookMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WebhookMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "async" => Ok(Self::Async),
            "sync" => Ok(Self::Sync),
            other => Err(other.to_string()),
        }
    }
}

/// Who may react to comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionsMode {
    Admin,
    Anyone,
}

impl ReactionsMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Anyone => "anyone",
        }
    }
}

impl std::fmt::Display for ReactionsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReactionsMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "admin" => Ok(Self::Admin),
            "anyone" => Ok(Self::Anyone),
            other => Err(other.to_string()),
        }
    }
}

/// Policy for undetectable / emoji-heavy comments. The single definition —
/// [`crate::language::LanguageGate`] consumes this instead of re-parsing a
/// string (its old `expect` is gone with the string field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmojiPolicy {
    /// Emoji-only comments are always accepted (default).
    #[default]
    Always,
    /// Emoji-heavy comments are rejected.
    Never,
    /// Emoji-heavy comments are accepted; other undetectable text is rejected.
    IfUnknown,
}

impl EmojiPolicy {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
            Self::IfUnknown => "if_unknown",
        }
    }
}

impl std::fmt::Display for EmojiPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EmojiPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            "if_unknown" => Ok(Self::IfUnknown),
            other => Err(other.to_string()),
        }
    }
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

fn validate_public_target_origin(raw: &str) -> Result<url::Url, ConfigError> {
    match url::Url::parse(raw) {
        Ok(parsed)
            if (parsed.scheme() == "http" || parsed.scheme() == "https")
                && parsed.host_str().is_some() =>
        {
            Ok(parsed)
        }
        _ => Err(ConfigError::InvalidPublicTargetOrigin(raw.to_string())),
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bind_addr: "127.0.0.1:3000".parse().expect("default bind_addr valid"),
            public_target_origin: "https://nithitsuki.com"
                .parse()
                .expect("default public_target_origin valid"),
            allowed_cors_origin: "https://nithitsuki.com".to_string(),
            admin_token: String::new(),
            database_path: "./comments.db".to_string(),
            db_quick_check: true,
            trust_proxy: false,
            github_token: None,
            max_content_len: 2000,
            max_author_len: 100,
            max_body_size: 8192,
            fetch_timeout_ms: 4000,
            worker_backlog: 64,
            honeypot_field: "website".to_string(),
            moderation_webhook_url: None,
            moderation_webhook_mode: WebhookMode::Async,
            webhook_signing_secret: None,
            default_comment_status: crate::moderation::Status::Pending,
            max_thread_depth: 0,
            turnstile_enabled: false,
            turnstile_secret_key: None,
            turnstile_verify_url: crate::turnstile::default_verify_url().to_string(),
            store_ip_address: false,
            ip_hash_secret: None,
            max_comments_per_ip_per_day: 50,
            max_webmentions_per_domain_per_hour: 10,
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
            notify_batch_granularity: BatchGranularity::Page,
            reactions_allowed: ReactionsMode::Admin,
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
            comment_lang_allow_emoji: EmojiPolicy::Always,
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
            defaults.public_target_origin.as_str(),
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

        let honeypot_field = env_or_default("HONEYPOT_FIELD", &defaults.honeypot_field);
        // Fail-closed: the configured name is substituted into the served
        // widget JS inside a single-quoted string. Anything outside
        // [A-Za-z0-9_-]{1,64} (an operator typo, a tag, a newline) must refuse
        // boot rather than ship a broken or breakable widget.
        if honeypot_field.len() > 64
            || honeypot_field.is_empty()
            || !honeypot_field
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ConfigError::InvalidHoneypotField(honeypot_field));
        }
        // Fail-loud like every other numeric knob: garbage refuses boot
        // instead of silently running on the default.
        let max_comments_per_ip_per_day = parse_or_err(
            "MAX_COMMENTS_PER_IP_PER_DAY",
            env_or_default(
                "MAX_COMMENTS_PER_IP_PER_DAY",
                &defaults.max_comments_per_ip_per_day.to_string(),
            ),
            ConfigError::InvalidMaxCommentsPerIpPerDay,
        )?;
        let max_webmentions_per_domain_per_hour = parse_or_err(
            "MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR",
            env_or_default(
                "MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR",
                &defaults.max_webmentions_per_domain_per_hour.to_string(),
            ),
            ConfigError::InvalidMaxWebmentionsPerDomainPerHour,
        )?;

        let store_ip_address = env_bool("STORE_IP_ADDRESS", defaults.store_ip_address);
        let ip_hash_secret = env::var("IP_HASH_SECRET").ok().filter(|s| !s.is_empty());

        let moderation_webhook_url = env::var("MODERATION_WEBHOOK_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let moderation_webhook_mode = env_or_default(
            "MODERATION_WEBHOOK_MODE",
            defaults.moderation_webhook_mode.as_str(),
        )
        .parse::<WebhookMode>()
        .map_err(ConfigError::InvalidModerationWebhookMode)?;
        let webhook_signing_secret = env::var("WEBHOOK_SIGNING_SECRET")
            .ok()
            .filter(|s| !s.is_empty());
        let default_comment_status_raw = env_or_default(
            "DEFAULT_COMMENT_STATUS",
            defaults.default_comment_status.as_str(),
        );
        let default_comment_status = match default_comment_status_raw.to_lowercase().as_str() {
            "pending" => crate::moderation::Status::Pending,
            "approved" => crate::moderation::Status::Approved,
            _ => {
                return Err(ConfigError::InvalidDefaultStatus(
                    default_comment_status_raw,
                ));
            }
        };
        let max_thread_depth = parse_or_err(
            "MAX_THREAD_DEPTH",
            env_or_default("MAX_THREAD_DEPTH", &defaults.max_thread_depth.to_string()),
            ConfigError::InvalidMaxThreadDepth,
        )
        .map(|v: i64| v.clamp(0, 10))?;

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
            defaults.notify_batch_granularity.as_str(),
        )
        .parse::<BatchGranularity>()
        .map_err(ConfigError::InvalidNotifyGranularity)?;

        let reactions_allowed =
            env_or_default("REACTIONS_ALLOWED", defaults.reactions_allowed.as_str())
                .parse::<ReactionsMode>()
                .map_err(ConfigError::InvalidReactionsMode)?;
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
            defaults.comment_lang_allow_emoji.as_str(),
        )
        .parse::<EmojiPolicy>()
        .map_err(ConfigError::InvalidEmojiPolicy)?;

        Ok(Config {
            bind_addr,
            public_target_origin,
            allowed_cors_origin,
            admin_token,
            database_path,
            db_quick_check: env_bool("DB_QUICK_CHECK", defaults.db_quick_check),
            github_token,
            max_content_len,
            max_author_len,
            max_body_size,
            fetch_timeout_ms,
            worker_backlog,
            honeypot_field,
            max_comments_per_ip_per_day,
            max_webmentions_per_domain_per_hour,
            store_ip_address,
            ip_hash_secret,
            moderation_webhook_url,
            moderation_webhook_mode,
            webhook_signing_secret,
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
            trust_proxy: env_bool("TRUST_PROXY", defaults.trust_proxy),
        })
    }

    pub fn redacted_display(&self) -> RedactedConfig<'_> {
        RedactedConfig(self)
    }

    /// Production moderation sink for the webmention worker's gone path:
    /// `None` when no webhook URL is configured, else the signed
    /// `status_sink` (the same constructor the comment/reaction paths build
    /// per request). [`crate::state::AppState::start`] and the F1
    /// prod-wiring test share this so the spawn path cannot diverge.
    #[must_use]
    pub fn worker_moderation_sink(
        &self,
        client: &reqwest::Client,
    ) -> Option<crate::moderation::WebhookSink> {
        self.moderation_webhook_url.as_ref().map(|url| {
            crate::moderation::WebhookSink::status_sink_signed(
                client,
                url,
                self.webhook_signing_secret.clone(),
            )
        })
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
                db_quick_check: {}, \
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
                webhook_signing_secret: {}, \
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
                trust_proxy: {} \
            }}",
            self.0.bind_addr,
            self.0.public_target_origin,
            self.0.allowed_cors_origin,
            self.0.database_path,
            self.0.db_quick_check,
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
            if self.0.webhook_signing_secret.is_some() {
                "***"
            } else {
                "(unset)"
            },
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
            self.0.trust_proxy,
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
        "DB_QUICK_CHECK",
        "GITHUB_TOKEN",
        "MAX_CONTENT_LEN",
        "MAX_AUTHOR_LEN",
        "MAX_BODY_SIZE",
        "FETCH_TIMEOUT_MS",
        "WORKER_BACKLOG",
        "HONEYPOT_FIELD",
        "MAX_COMMENTS_PER_IP_PER_DAY",
        "MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR",
        "STORE_IP_ADDRESS",
        "IP_HASH_SECRET",
        "MODERATION_WEBHOOK_URL",
        "MODERATION_WEBHOOK_MODE",
        "WEBHOOK_SIGNING_SECRET",
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
        "TRUST_PROXY",
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
        // Ambient environment (e.g. DATABASE_PATH exported in CI) cannot
        // leak into `from_env`.
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
            assert_eq!(
                config.public_target_origin.as_str(),
                "https://nithitsuki.com/"
            );
            assert_eq!(config.allowed_cors_origin, "https://nithitsuki.com");
            assert_eq!(config.admin_token, "test-token");
            assert_eq!(config.database_path, "./comments.db");
            assert!(config.github_token.is_none());
            assert_eq!(config.max_content_len, 2000);
            assert_eq!(config.max_author_len, 100);
            assert_eq!(config.max_body_size, 8192);
            assert_eq!(config.fetch_timeout_ms, 4000);
            assert_eq!(config.worker_backlog, 64);
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
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(config.bind_addr.to_string(), "0.0.0.0:9090");
                assert_eq!(config.public_target_origin.as_str(), "https://example.com/");
                assert_eq!(config.allowed_cors_origin, "https://example.com");
                assert_eq!(config.admin_token, "my-secret");
                assert_eq!(config.database_path, "/data/comments.db");
                assert_eq!(config.github_token, Some("ghp_xxx".to_string()));
                assert_eq!(config.max_content_len, 1000);
                assert_eq!(config.max_author_len, 50);
                assert_eq!(config.max_body_size, 4096);
                assert_eq!(config.fetch_timeout_ms, 2000);
                assert_eq!(config.worker_backlog, 128);
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
            assert_eq!(config.notify_batch_granularity, BatchGranularity::Page);
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
                assert_eq!(config.notify_batch_granularity, BatchGranularity::Global);
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
            assert_eq!(config.reactions_allowed, ReactionsMode::Admin);
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
                assert_eq!(config.reactions_allowed, ReactionsMode::Anyone);
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
            assert_eq!(config.comment_lang_allow_emoji, EmojiPolicy::Always);
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
                assert_eq!(config.comment_lang_allow_emoji, EmojiPolicy::IfUnknown);
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
    fn webhook_signing_secret_defaults_unset_and_loads() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            assert!(
                Config::from_env().unwrap().webhook_signing_secret.is_none(),
                "unsigned by default (backwards compatible)"
            );
        });
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("WEBHOOK_SIGNING_SECRET", "s3cr3t"),
            ],
            || {
                assert_eq!(
                    Config::from_env()
                        .unwrap()
                        .webhook_signing_secret
                        .as_deref(),
                    Some("s3cr3t")
                );
            },
        );
        with_env(
            &[("ADMIN_TOKEN", "test"), ("WEBHOOK_SIGNING_SECRET", "")],
            || {
                assert!(
                    Config::from_env().unwrap().webhook_signing_secret.is_none(),
                    "empty secret means unsigned"
                );
            },
        );
    }

    #[test]
    fn webhook_signing_secret_redacted_in_display() {
        let config = Config {
            webhook_signing_secret: Some("s3cr3t".to_string()),
            ..Config::default()
        };
        let rendered = format!("{}", config.redacted_display());
        assert!(!rendered.contains("s3cr3t"), "signing secret leaked");
        assert!(
            rendered.contains("webhook_signing_secret: ***"),
            "signing secret not redacted: {rendered}"
        );
    }

    #[test]
    fn honeypot_field_restricted_to_safe_names() {
        // Fail-closed: hostile or typo'd names must refuse boot, not ship a
        // broken widget (</script>/newline breakout class).
        let mut bads: Vec<String> = [
            "x</script><script>alert(1)</script>",
            "a\nb",
            "a'b",
            "a\"b",
            "a\\b",
            "a b",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        bads.push("a".repeat(65));
        for bad in &bads {
            with_env(&[("ADMIN_TOKEN", "test"), ("HONEYPOT_FIELD", bad)], || {
                let err = Config::from_env().unwrap_err();
                assert!(
                    matches!(err, ConfigError::InvalidHoneypotField(_)),
                    "HONEYPOT_FIELD={bad:?} must fail, got: {err}"
                );
            });
        }
        let mut goods: Vec<String> = ["website", "company", "a-b_c9"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        goods.push("a".repeat(64));
        for good in &goods {
            with_env(&[("ADMIN_TOKEN", "test"), ("HONEYPOT_FIELD", good)], || {
                assert_eq!(Config::from_env().unwrap().honeypot_field, *good);
            });
        }
    }

    #[test]
    fn trust_proxy_defaults_off_and_parses_bool() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            assert!(
                !Config::from_env().unwrap().trust_proxy,
                "proxy headers must be ignored unless TRUST_PROXY is set"
            );
        });
        for val in ["true", "TRUE", "1"] {
            with_env(&[("ADMIN_TOKEN", "test"), ("TRUST_PROXY", val)], || {
                assert!(
                    Config::from_env().unwrap().trust_proxy,
                    "TRUST_PROXY={val} must enable proxy-header trust"
                );
            });
        }
        with_env(&[("ADMIN_TOKEN", "test"), ("TRUST_PROXY", "false")], || {
            assert!(!Config::from_env().unwrap().trust_proxy);
        });
    }

    #[test]
    fn redacted_display_hides_admin_and_github_tokens() {
        let config = Config {
            bind_addr: "127.0.0.1:3000".parse().unwrap(),
            public_target_origin: "https://nithitsuki.com".parse().unwrap(),
            allowed_cors_origin: "https://nithitsuki.com".to_string(),
            admin_token: "super-secret-12345".to_string(),
            database_path: "./comments.db".to_string(),
            github_token: Some("ghp_xxx".to_string()),
            max_content_len: 2000,
            max_author_len: 100,
            max_body_size: 8192,
            fetch_timeout_ms: 4000,
            worker_backlog: 64,
            honeypot_field: "website".to_string(),
            max_comments_per_ip_per_day: 50,
            max_webmentions_per_domain_per_hour: 10,
            store_ip_address: false,
            ip_hash_secret: None,
            moderation_webhook_url: None,
            moderation_webhook_mode: WebhookMode::Async,
            webhook_signing_secret: None,
            default_comment_status: crate::moderation::Status::Pending,
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
            notify_batch_granularity: BatchGranularity::Page,
            reactions_allowed: ReactionsMode::Admin,
            reactions_set: vec!["👍".to_string(), "❤️".to_string()],
            comment_lang_allowed: vec!["en".to_string()],
            comment_lang_blocked: Vec::new(),
            comment_lang_allow_emoji: EmojiPolicy::Always,
            db_quick_check: true,
            trust_proxy: false,
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
                assert_eq!(config.public_target_origin.as_str(), "https://example.com/");
            },
        );
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("PUBLIC_TARGET_ORIGIN", "http://localhost:3000"),
            ],
            || {
                let config = Config::from_env().unwrap();
                assert_eq!(
                    config.public_target_origin.as_str(),
                    "http://localhost:3000/"
                );
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
        // Fail-loud audit pin: these three fields once fell back to defaults
        // on garbage (`unwrap_or`); they now refuse boot like every other
        // numeric knob. They must still read their defaults from
        // `Config::default()` — a drifted literal here would silently diverge
        // from `Default`.
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
    fn db_quick_check_defaults_on() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert!(
                config.db_quick_check,
                "DB_QUICK_CHECK must default ON (corruption detection beats boot latency)"
            );
        });
    }

    #[test]
    fn db_quick_check_opt_out() {
        with_env(
            &[("ADMIN_TOKEN", "test"), ("DB_QUICK_CHECK", "false")],
            || {
                let config = Config::from_env().unwrap();
                assert!(
                    !config.db_quick_check,
                    "DB_QUICK_CHECK=false must disable the startup check"
                );
            },
        );
        with_env(&[("ADMIN_TOKEN", "test"), ("DB_QUICK_CHECK", "0")], || {
            let config = Config::from_env().unwrap();
            assert!(
                !config.db_quick_check,
                "DB_QUICK_CHECK=0 must disable the startup check"
            );
        });
    }

    #[test]
    fn batch_granularity_parses_once_and_rejects_garbage() {
        for (raw, expected) in [
            ("page", BatchGranularity::Page),
            ("global", BatchGranularity::Global),
            ("PAGE", BatchGranularity::Page),
        ] {
            with_env(
                &[("ADMIN_TOKEN", "test"), ("NOTIFY_BATCH_GRANULARITY", raw)],
                || {
                    assert_eq!(
                        Config::from_env().unwrap().notify_batch_granularity,
                        expected
                    );
                },
            );
        }
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("NOTIFY_BATCH_GRANULARITY", "per-comment"),
            ],
            || {
                assert!(matches!(
                    Config::from_env().unwrap_err(),
                    ConfigError::InvalidNotifyGranularity(_)
                ));
            },
        );
    }

    #[test]
    fn webhook_mode_parses_once_and_rejects_garbage() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            assert_eq!(
                Config::from_env().unwrap().moderation_webhook_mode,
                WebhookMode::Async
            );
        });
        for (raw, expected) in [
            ("async", WebhookMode::Async),
            ("sync", WebhookMode::Sync),
            ("SYNC", WebhookMode::Sync),
        ] {
            with_env(
                &[("ADMIN_TOKEN", "test"), ("MODERATION_WEBHOOK_MODE", raw)],
                || {
                    assert_eq!(
                        Config::from_env().unwrap().moderation_webhook_mode,
                        expected
                    );
                },
            );
        }
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("MODERATION_WEBHOOK_MODE", "sometimes"),
            ],
            || {
                assert!(matches!(
                    Config::from_env().unwrap_err(),
                    ConfigError::InvalidModerationWebhookMode(_)
                ));
            },
        );
    }

    #[test]
    fn default_status_parses_once_and_rejects_garbage() {
        use crate::moderation::Status;
        with_env(&[("ADMIN_TOKEN", "test")], || {
            assert_eq!(
                Config::from_env().unwrap().default_comment_status,
                Status::Pending
            );
        });
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("DEFAULT_COMMENT_STATUS", "APPROVED"),
            ],
            || {
                assert_eq!(
                    Config::from_env().unwrap().default_comment_status,
                    Status::Approved
                );
            },
        );
        for bad in ["sometimes", "spam", "deleted", ""] {
            with_env(
                &[("ADMIN_TOKEN", "test"), ("DEFAULT_COMMENT_STATUS", bad)],
                || {
                    assert!(
                        matches!(
                            Config::from_env().unwrap_err(),
                            ConfigError::InvalidDefaultStatus(_)
                        ),
                        "DEFAULT_COMMENT_STATUS={bad:?} must fail loud"
                    );
                },
            );
        }
    }

    #[test]
    fn quota_and_depth_garbage_fails_loud() {
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("MAX_COMMENTS_PER_IP_PER_DAY", "garbage"),
            ],
            || {
                assert!(matches!(
                    Config::from_env().unwrap_err(),
                    ConfigError::InvalidMaxCommentsPerIpPerDay(_)
                ));
            },
        );
        with_env(
            &[
                ("ADMIN_TOKEN", "test"),
                ("MAX_WEBMENTIONS_PER_DOMAIN_PER_HOUR", "garbage"),
            ],
            || {
                assert!(matches!(
                    Config::from_env().unwrap_err(),
                    ConfigError::InvalidMaxWebmentionsPerDomainPerHour(_)
                ));
            },
        );
        with_env(
            &[("ADMIN_TOKEN", "test"), ("MAX_THREAD_DEPTH", "garbage")],
            || {
                assert!(matches!(
                    Config::from_env().unwrap_err(),
                    ConfigError::InvalidMaxThreadDepth(_)
                ));
            },
        );
    }

    #[test]
    fn public_target_origin_is_typed_url() {
        with_env(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(
                config.public_target_origin.as_str(),
                "https://nithitsuki.com/"
            );
        });
    }

    #[test]
    fn worker_moderation_sink_absent_without_url() {
        let client = reqwest::Client::new();
        assert!(
            Config::default().worker_moderation_sink(&client).is_none(),
            "no webhook URL means no worker sink"
        );
        let config = Config {
            moderation_webhook_url: Some("https://mod.example/hook".to_string()),
            webhook_signing_secret: Some("s3cr3t".to_string()),
            ..Config::default()
        };
        assert!(
            config.worker_moderation_sink(&client).is_some(),
            "configured URL means the worker carries a sink"
        );
    }

    #[test]
    fn with_env_is_hermetic_against_ambient_vars() {
        // Simulate ambient pollution (e.g. CI exporting DATABASE_PATH):
        // `with_env` must clear it for the duration and restore it after.
        // The whole sequence holds ENV_LOCK so no other env test can
        // interleave between the pollution and the hermetic window.
        let _lock = ENV_LOCK.lock().unwrap();
        let ambient_before = env::var("DATABASE_PATH").ok();
        // SAFETY: ENV_LOCK is held.
        unsafe { env::set_var("DATABASE_PATH", "polluted-by-ambient") };
        with_env_locked(&[("ADMIN_TOKEN", "test")], || {
            let config = Config::from_env().unwrap();
            assert_eq!(
                config.database_path, "./comments.db",
                "ambient DATABASE_PATH must not leak into from_env"
            );
        });
        assert_eq!(
            env::var("DATABASE_PATH").as_deref(),
            Ok("polluted-by-ambient"),
            "with_env must restore ambient vars afterwards"
        );
        // Restore the pre-existing state so we don't pollute other tests.
        // SAFETY: ENV_LOCK is held.
        unsafe {
            match ambient_before {
                Some(v) => env::set_var("DATABASE_PATH", v),
                None => env::remove_var("DATABASE_PATH"),
            }
        }
    }
}
