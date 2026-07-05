use axum::Form;
use axum::extract::State;
use serde::Deserialize;
use tokio::sync::mpsc::error::TrySendError;
use url::Url;

use crate::error::AppError;
use crate::state::{domain_hourly_key, AppState};
use crate::worker::WebmentionJob;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct WebmentionForm {
    /// URL of the page linking to your site
    pub source: String,
    /// URL on your site that is being linked to
    pub target: String,
}

#[utoipa::path(
    post,
    path = "/api/webmention",
    request_body(content = WebmentionForm, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 202, description = "Webmention accepted for processing"),
        (status = 400, description = "Invalid source/target or target origin mismatch"),
        (status = 503, description = "Worker backlog full, retry later"),
    ),
    tag = "webmention",
)]
pub async fn receive_webmention(
    State(state): State<AppState>,
    Form(form): Form<WebmentionForm>,
) -> Result<(axum::http::StatusCode, &'static str), AppError> {
    // 1. Validate source and target are absolute http(s) URLs.
    let source_url = Url::parse(&form.source)
        .map_err(|_| AppError::BadRequest("invalid source URL".to_string()))?;
    if source_url.scheme() != "http" && source_url.scheme() != "https" {
        return Err(AppError::BadRequest(
            "source URL must be http or https".to_string(),
        ));
    }

    // 1b. Per-domain hourly cap for webmentions.
    if state.config.max_webmentions_per_domain_per_hour > 0 {
        if let Some(host) = source_url.host_str() {
            let key = domain_hourly_key(host);
            if !state.limiter.check_and_increment(&key, state.config.max_webmentions_per_domain_per_hour) {
                return Err(AppError::RateLimited {
                    retry_after_secs: 3600,
                    reason: format!(
                        "hourly webmention limit ({}) reached for domain '{host}'",
                        state.config.max_webmentions_per_domain_per_hour,
                    ),
                });
            }
        }
    }

    let target_url = Url::parse(&form.target)
        .map_err(|_| AppError::BadRequest("invalid target URL".to_string()))?;
    if target_url.scheme() != "http" && target_url.scheme() != "https" {
        return Err(AppError::BadRequest(
            "target URL must be http or https".to_string(),
        ));
    }

    // 2. Target must match the configured origin by URL origin, not string prefix.
    let target_origin_url = Url::parse(&state.config.public_target_origin)
        .expect("PUBLIC_TARGET_ORIGIN validated at config load");
    if target_url.origin() != target_origin_url.origin() {
        return Err(AppError::BadRequest(format!(
            "target origin '{}' does not match configured origin",
            target_url.origin().ascii_serialization(),
        )));
    }

    // 3. Source and target must differ.
    if form.source == form.target {
        return Err(AppError::BadRequest(
            "source and target must differ".to_string(),
        ));
    }

    // 4. Enqueue.
    let job = WebmentionJob {
        source: form.source,
        target: form.target,
    };

    match state.wm_sender.try_send(job) {
        Ok(()) => Ok((axum::http::StatusCode::ACCEPTED, "accepted")),
        Err(TrySendError::Full(_)) => Err(AppError::ServiceUnavailable(
            "worker backlog full, retry later".to_string(),
        )),
        Err(TrySendError::Closed(_)) => Err(AppError::Internal(
            "webmention worker is not running".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::http::build_app;
    use crate::http::test_support::helpers;
    use crate::state::{AppState, Limiter};
    use crate::worker;

    fn test_state() -> (AppState, tempfile::TempDir) {
        helpers::test_state()
    }

    fn form_request(body: &str) -> axum::http::Request<axum::body::Body> {
        helpers::form_request("/api/webmention", body)
    }

    #[tokio::test]
    async fn valid_ping_returns_202() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "source=https://remote.example/post&target=https://nithitsuki.com/blog/hello";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 202);
    }

    #[tokio::test]
    async fn target_not_from_origin_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "source=https://remote.example/post&target=https://evil.com/hack";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn source_equal_target_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "source=https://nithitsuki.com/same&target=https://nithitsuki.com/same";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn source_not_http_url_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "source=not-a-url&target=https://nithitsuki.com/x";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn target_not_http_url_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let body = "source=https://remote.example/post&target=ftp://nithitsuki.com/x";
        let resp = app.oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn backlog_full_returns_503() {
        use crate::config::Config;
        use crate::db::pool::{create_pool, run_migrations};
        use crate::db::repo::CommentsRepo;

        let (wm_sender, _rx) = worker::channel(1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool).unwrap();
        let repo = CommentsRepo::new(pool.clone());
        let state = AppState {
            config: Config {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                public_target_origin: "https://nithitsuki.com".to_string(),
                allowed_cors_origin: "https://nithitsuki.com".to_string(),
                admin_token: "test".to_string(),
                database_path: ":memory:".to_string(),
                github_token: None,
                max_content_len: 2000,
                max_author_len: 100,
                max_body_size: 8192,
                fetch_timeout_ms: 4000,
                worker_backlog: 1,
                rust_log: "info".to_string(),
            honeypot_field: "website".to_string(),
            max_comments_per_ip_per_day: 50,
            max_webmentions_per_domain_per_hour: 10,
            store_ip_address: false,
            moderation_webhook_url: None,
            default_comment_status: "pending".to_string(),
            max_thread_depth: 0,
            },
            pool,
            repo,
            github: Arc::new(crate::github::StubGitHub),
            wm_sender,
            http_client: { reqwest::Client::builder().build().unwrap() },
            limiter: Arc::new(Limiter::new()),
        };
        let app = build_app(state);

        // First request fills the channel (worker hasn't consumed it).
        let body = "source=https://a.example/post&target=https://nithitsuki.com/x";
        let resp = app.clone().oneshot(form_request(body)).await.unwrap();
        assert_eq!(resp.status(), 202);

        // Second request should fail because the channel is full.
        let body2 = "source=https://b.example/post&target=https://nithitsuki.com/x";
        let resp = app.clone().oneshot(form_request(body2)).await.unwrap();
        assert_eq!(resp.status(), 503);
    }
}
