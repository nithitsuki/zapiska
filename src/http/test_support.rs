#[cfg(test)]
pub(crate) mod helpers {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::config::Config;
    use crate::github::{GitHubLookup, StubGitHub};
    use crate::state::AppState;

    /// A test-ready AppState + a TempDir that keeps the DB file alive.
    pub fn test_state() -> (AppState, TempDir) {
        test_state_with_github(Arc::new(StubGitHub))
    }

    /// Like `test_state` but with a custom GitHubLookup implementation.
    pub fn test_state_with_github(github: Arc<dyn GitHubLookup>) -> (AppState, TempDir) {
        build_state(github, false, None, default_verify_url())
    }

    /// Like `test_state` but with Turnstile verification enabled, pointing the
    /// siteverify call at a custom URL (typically a wiremock server URI).
    pub fn test_state_with_turnstile(verify_url: String) -> (AppState, TempDir) {
        build_state(
            Arc::new(StubGitHub),
            true,
            Some("test-secret".to_string()),
            verify_url,
        )
    }

    fn default_verify_url() -> String {
        crate::turnstile::default_verify_url().to_string()
    }

    fn build_state(
        github: Arc<dyn GitHubLookup>,
        turnstile_enabled: bool,
        turnstile_secret: Option<String>,
        turnstile_verify_url: String,
    ) -> (AppState, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("test.db");
        // All assembly rides AppState::start_with_github (the same path as
        // production AppState::start, minus the GitHub adapter): pool,
        // migrations, notifier, language gate, and the real worker.
        let config = Config {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            admin_token: "test".to_string(),
            database_path: path.to_string_lossy().to_string(),
            turnstile_enabled,
            turnstile_secret_key: turnstile_secret,
            turnstile_verify_url,
            rate_limit_native_burst: 50,
            rate_limit_webmention_burst: 30,
            rate_limit_read_burst: 60,
            rate_limit_admin_moderate_burst: 10,
            notify_batch_secs: 0,
            notify_batch_threshold: 0,
            reactions_set: vec!["👍".to_string(), "❤️".to_string(), "😄".to_string()],
            ..Config::default()
        };
        let state = AppState::start_with_github(config, github).expect("test start");
        (state, dir)
    }

    /// Like `test_state` but with Telegram and Slack notifications pointed at
    /// the given endpoints (typically wiremock servers). Notifications are
    /// immediate (`NOTIFY_BATCH_SECS = 0`).
    pub fn test_state_with_notifications(
        telegram_api_base: String,
        slack_webhook_url: Option<String>,
    ) -> (AppState, TempDir) {
        let (mut state, dir) = test_state();
        state.config.telegram_bot_token = Some("TESTTOKEN123:test-secret".to_string());
        state.config.telegram_chat_id = Some("@test_alerts".to_string());
        state.config.telegram_api_base = telegram_api_base;
        state.config.slack_webhook_url = slack_webhook_url;
        state.notifier = Arc::new(crate::notify::NotificationBatcher::new(&state.config));
        (state, dir)
    }

    /// Like `test_state` but with batched notifications: the given window and
    /// threshold, Telegram pointed at `telegram_api_base`, plus Discord when
    /// `discord_webhook_url` is provided.
    pub fn test_state_with_batcher(
        batch_secs: u64,
        batch_threshold: u32,
        granularity: &str,
        telegram_api_base: String,
        discord_webhook_url: Option<String>,
    ) -> (AppState, TempDir) {
        let (mut state, dir) = test_state();
        state.config.telegram_bot_token = Some("TESTTOKEN123:test-secret".to_string());
        state.config.telegram_chat_id = Some("@test_alerts".to_string());
        state.config.telegram_api_base = telegram_api_base;
        state.config.notify_batch_secs = batch_secs;
        state.config.notify_batch_threshold = batch_threshold;
        state.config.notify_batch_granularity =
            granularity.parse().expect("test granularity valid");
        state.config.discord_webhook_url = discord_webhook_url;
        state.notifier = Arc::new(crate::notify::NotificationBatcher::new(&state.config));
        (state, dir)
    }

    /// Helper to build an HTTP request for testing.
    pub fn request(method: axum::http::Method, uri: &str) -> axum::http::Request<axum::body::Body> {
        let is_write = method == axum::http::Method::POST || method == axum::http::Method::PUT;
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .extension(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))))
            .body(axum::body::Body::empty())
            .unwrap();
        if is_write {
            req.headers_mut()
                .insert(axum::http::header::CONTENT_LENGTH, 0u64.into());
        }
        req
    }

    pub fn form_request(uri: &str, body: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header(
                axum::http::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(axum::http::header::CONTENT_LENGTH, body.len())
            .extension(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))))
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap()
    }

    /// Helper to build an admin-authorized JSON request for testing
    /// (Bearer `test` — matches the test state's ADMIN_TOKEN).
    pub fn json_request(
        method: axum::http::Method,
        uri: &str,
        body: &str,
    ) -> axum::http::Request<axum::body::Body> {
        let mut req = request(method, uri);
        *req.body_mut() = axum::body::Body::from(body.to_owned());
        req.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        req.headers_mut()
            .insert(axum::http::header::CONTENT_LENGTH, body.len().into());
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test"),
        );
        req
    }
}
