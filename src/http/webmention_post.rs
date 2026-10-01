use axum::Form;
use axum::extract::State;
use serde::Deserialize;
use tokio::sync::mpsc::error::TrySendError;
use url::Url;

use crate::error::AppError;
use crate::state::{AppState, domain_hourly_key};
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

    // The per-domain hourly cap runs as step 4, after every check that can
    // reject the request. See the comment there.
    let target_url = Url::parse(&form.target)
        .map_err(|_| AppError::BadRequest("invalid target URL".to_string()))?;
    if target_url.scheme() != "http" && target_url.scheme() != "https" {
        return Err(AppError::BadRequest(
            "target URL must be http or https".to_string(),
        ));
    }

    // 2. Target must match the configured origin by URL origin, not string prefix.
    let target_origin_url = state.config.public_target_origin.clone();
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

    // 4. Enqueue first, then charge the per-domain quota.
    //
    // Enqueue BEFORE the charge so the two failure paths are separable. A
    // full backlog (503) must not cost the sender anything: the request was
    // never accepted, and an unauthenticated caller could otherwise keep the
    // queue saturated and burn a victim's hourly budget on every rejected
    // ping.
    //
    // The charge must also follow every check that can reject the request.
    // The old order (charge first) meant a caller sending
    // `source=<victim-host>/x` with an invalid target got 400 and still paid
    // the victim's budget, after which that domain's real webmentions met 429.
    let job = WebmentionJob {
        source: form.source,
        target: form.target,
    };

    state.wm_sender.try_send(job).map_err(|e| match e {
        TrySendError::Full(_) => {
            AppError::ServiceUnavailable("worker backlog full, retry later".to_string())
        }
        TrySendError::Closed(_) => {
            AppError::Internal("webmention worker is not running".to_string())
        }
    })?;

    // 5. Charge the quota, last. Every refusal path above returns before
    // this point, so only an accepted ping costs the source domain a slot.
    //
    // The key is the source host, which is unauthenticated. A caller can
    // still spend its OWN domain's budget freely by naming itself as the
    // source of an otherwise-valid ping; the worker later drops a ping with
    // no backlink. That is the intended cost of the cap.
    if state.config.max_webmentions_per_domain_per_hour > 0 {
        if let Some(host) = source_url.host_str() {
            let key = domain_hourly_key(host);
            if !state
                .limiter
                .check_and_increment(&key, state.config.max_webmentions_per_domain_per_hour)
            {
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

    Ok((axum::http::StatusCode::ACCEPTED, "accepted"))
}

/// W3C webmention discovery document. The body is the relative receipt path
/// (`/api/webmention`): the consuming site's own static file carries the
/// absolute URL, so this origin never needs to know its public hostname and
/// no configuration is added.
#[utoipa::path(
    get,
    path = "/.well-known/webmention",
    responses(
        (status = 200, description = "Relative webmention receipt path", content_type = "text/plain", body = String),
    ),
    tag = "webmention",
)]
pub async fn well_known_webmention() -> (axum::http::StatusCode, axum::http::HeaderMap, &'static str)
{
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/plain".parse().expect("static header valid"),
    );
    (axum::http::StatusCode::OK, headers, "/api/webmention")
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
        // Intentional exception to the AppState::start rule: forcing the
        // 503 needs an undrained channel(1) nobody receives from, while
        // start_with_github spawns a draining worker. Compiles against
        // the typed Config via ..Config::default().
        use crate::config::Config;
        use crate::db::pool::{create_pool, run_migrations};
        use crate::db::repo::Repo;

        let (wm_sender, _rx) = worker::channel(1);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("full.db");
        let pool = create_pool(&path.to_string_lossy()).unwrap();
        run_migrations(&pool, None).unwrap();
        let repo = Repo::new(pool.clone());
        let state = AppState {
            config: Config {
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                admin_token: "test".to_string(),
                database_path: ":memory:".to_string(),
                worker_backlog: 1,
                rate_limit_native_burst: 50,
                rate_limit_webmention_burst: 30,
                rate_limit_read_burst: 60,
                rate_limit_admin_moderate_burst: 10,
                notify_batch_secs: 0,
                notify_batch_threshold: 0,
                reactions_set: vec!["👍".to_string()],
                ..Config::default()
            },
            pool,
            repo,
            github: Arc::new(crate::github::StubGitHub),
            notifier: Arc::new(crate::notify::NotificationBatcher::default()),
            language: crate::language::LanguageGate::default(),
            wm_sender,
            wm_shutdown: tokio::sync::watch::channel(false).0,
            wm_worker: Arc::new(std::sync::Mutex::new(None)),
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
