//! Admin status endpoint: versions, health, and non-secret configuration in
//! one place for the dashboard OPS tab. Every value is either a constant or
//! a boolean derived from config — secret VALUES (tokens, webhook URLs,
//! salts) never leave the server; see `redacted_display` for the same rule.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct AdminStatus {
    pub version: &'static str,
    pub schema_version: i64,
    pub export_version: i64,
    pub health: &'static str,
    pub honeypot_field: String,
    pub webhook: WebhookStatus,
    pub notify: NotifyStatus,
    pub trust_proxy: bool,
    pub reactions: ReactionsStatus,
    pub default_comment_status: String,
    pub max_thread_depth: i64,
    pub turnstile_enabled: bool,
    pub language: LanguageStatus,
    pub privacy: PrivacyStatus,
    pub limits: LimitsStatus,
    pub db_quick_check: bool,
    pub worker_backlog: usize,
}

#[derive(Serialize)]
pub struct WebhookStatus {
    pub configured: bool,
    pub mode: String,
    pub signed: bool,
}

#[derive(Serialize)]
pub struct NotifyStatus {
    pub telegram: bool,
    pub slack: bool,
    pub discord: bool,
    pub batch_secs: u64,
    pub batch_threshold: u32,
    pub batch_granularity: String,
}

#[derive(Serialize)]
pub struct ReactionsStatus {
    pub allowed: String,
    pub set: Vec<String>,
}

#[derive(Serialize)]
pub struct LanguageStatus {
    pub allowed: Vec<String>,
    pub blocked: Vec<String>,
    pub emoji: String,
}

#[derive(Serialize)]
pub struct PrivacyStatus {
    pub store_ip_address: bool,
    pub ip_hash_salted: bool,
}

#[derive(Serialize)]
pub struct LimitsStatus {
    pub native_comment_daily_cap: u32,
    pub webmention_domain_hourly_cap: u32,
    pub native_burst: u32,
    pub native_window_secs: u64,
    pub webmention_burst: u32,
    pub webmention_window_secs: u64,
    pub read_burst: u32,
    pub read_window_secs: u64,
    pub admin_moderate_burst: u32,
    pub admin_moderate_window_secs: u64,
}

/// GET /api/admin/status — versions, DB health, and non-secret config.
pub async fn status(State(state): State<AppState>) -> Json<AdminStatus> {
    let c = &state.config;
    Json(AdminStatus {
        version: crate::APP_VERSION,
        schema_version: crate::db::pool::LATEST_SCHEMA_VERSION,
        export_version: crate::http::admin::data::EXPORT_VERSION,
        health: if crate::http::db_is_healthy(&state.pool).await {
            "ok"
        } else {
            "unavailable"
        },
        honeypot_field: c.honeypot_field.clone(),
        webhook: WebhookStatus {
            configured: c.moderation_webhook_url.is_some(),
            mode: c.moderation_webhook_mode.to_string(),
            signed: c.webhook_signing_secret.is_some(),
        },
        notify: NotifyStatus {
            telegram: c.telegram_bot_token.is_some() && c.telegram_chat_id.is_some(),
            slack: c.slack_webhook_url.is_some(),
            discord: c.discord_webhook_url.is_some(),
            batch_secs: c.notify_batch_secs,
            batch_threshold: c.notify_batch_threshold,
            batch_granularity: c.notify_batch_granularity.to_string(),
        },
        trust_proxy: c.trust_proxy,
        reactions: ReactionsStatus {
            allowed: c.reactions_allowed.to_string(),
            set: c.reactions_set.clone(),
        },
        default_comment_status: c.default_comment_status.to_string(),
        max_thread_depth: c.max_thread_depth,
        turnstile_enabled: c.turnstile_enabled,
        language: LanguageStatus {
            allowed: c.comment_lang_allowed.clone(),
            blocked: c.comment_lang_blocked.clone(),
            emoji: c.comment_lang_allow_emoji.to_string(),
        },
        privacy: PrivacyStatus {
            store_ip_address: c.store_ip_address,
            ip_hash_salted: c.ip_hash_secret.is_some(),
        },
        limits: LimitsStatus {
            native_comment_daily_cap: c.max_comments_per_ip_per_day,
            webmention_domain_hourly_cap: c.max_webmentions_per_domain_per_hour,
            native_burst: c.rate_limit_native_burst,
            native_window_secs: c.rate_limit_native_window_secs,
            webmention_burst: c.rate_limit_webmention_burst,
            webmention_window_secs: c.rate_limit_webmention_window_secs,
            read_burst: c.rate_limit_read_burst,
            read_window_secs: c.rate_limit_read_window_secs,
            admin_moderate_burst: c.rate_limit_admin_moderate_burst,
            admin_moderate_window_secs: c.rate_limit_admin_moderate_window_secs,
        },
        db_quick_check: c.db_quick_check,
        worker_backlog: c.worker_backlog,
    })
}

#[cfg(test)]
mod tests {
    use tower::ServiceExt;

    use crate::http::build_app;
    use crate::http::test_support::helpers;

    fn authorized(uri: &str) -> axum::http::Request<axum::body::Body> {
        let mut req = helpers::request(axum::http::Method::GET, uri);
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test"),
        );
        req
    }

    #[tokio::test]
    async fn status_reports_versions_health_and_non_secret_config() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app.oneshot(authorized("/api/admin/status")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["version"], crate::APP_VERSION);
        assert_eq!(v["schema_version"], crate::db::pool::LATEST_SCHEMA_VERSION);
        assert_eq!(
            v["export_version"],
            crate::http::admin::data::EXPORT_VERSION
        );
        assert_eq!(v["health"], "ok");
        assert!(v["honeypot_field"].is_string());
        assert_eq!(v["webhook"]["configured"], false);
        assert_eq!(v["webhook"]["signed"], false);
        assert_eq!(v["trust_proxy"], false);
        assert!(v["reactions"]["set"].is_array());
        assert_eq!(v["privacy"]["ip_hash_salted"], false);
        // Secrets never leave the server, even as substrings.
        let raw = String::from_utf8_lossy(&body);
        assert!(
            !raw.contains("test-secret"),
            "admin token leaked into status"
        );
    }

    #[tokio::test]
    async fn status_reflects_configured_channels_without_values() {
        let (mut state, _dir) = helpers::test_state();
        state.config.slack_webhook_url = Some("https://hooks.example/x".to_string());
        state.config.telegram_bot_token = Some("tok".to_string());
        state.config.telegram_chat_id = Some("chat".to_string());
        state.config.moderation_webhook_url = Some("https://mod.example/hook".to_string());
        state.config.webhook_signing_secret = Some("s3cr3t".to_string());
        let app = build_app(state);
        let resp = app.oneshot(authorized("/api/admin/status")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 65536).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["notify"]["slack"], true);
        assert_eq!(v["notify"]["telegram"], true);
        assert_eq!(v["webhook"]["configured"], true);
        assert_eq!(v["webhook"]["signed"], true);
        let raw = String::from_utf8_lossy(&body);
        assert!(!raw.contains("hooks.example"), "webhook URL leaked");
        assert!(!raw.contains("s3cr3t"), "signing secret leaked");
    }

    #[tokio::test]
    async fn status_requires_auth() {
        let (state, _dir) = helpers::test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(helpers::request(
                axum::http::Method::GET,
                "/api/admin/status",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }
}
