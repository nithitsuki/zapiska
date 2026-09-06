pub(crate) mod admin;
pub(crate) mod comment_post;
pub(crate) mod comments_read;
pub(crate) mod feed;
mod layers;
pub(crate) mod reactions;
#[cfg(feature = "webmentions")]
pub mod reqwest_client;
pub mod shutdown;
#[cfg(test)]
pub(crate) mod test_support;
pub(crate) mod webhook;
#[cfg(feature = "webmentions")]
pub(crate) mod webmention_post;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::sync::Arc;
use tower_governor::GovernorLayer;
use utoipa::OpenApi;

use crate::openapi::ApiDoc;
use crate::state::AppState;

pub fn build_app(state: AppState) -> Router {
    let native_governor = Arc::new(layers::native_comment_governor(&state.config));
    #[cfg(feature = "webmentions")]
    let webmention_governor = layers::webmention_governor(&state.config);
    let read_governor = Arc::new(layers::read_governor(&state.config));
    let admin_moderate_governor = layers::admin_moderate_governor(&state.config);

    let cors = layers::cors_layer(&state.config);
    let body_limit = layers::body_limit_layer(&state.config);

    let admin_routes = Router::new()
        .route(
            "/api/admin/pending",
            axum::routing::get(admin::list_pending),
        )
        .route("/api/admin/paths", axum::routing::get(admin::list_paths))
        .route(
            "/api/admin/comments",
            axum::routing::get(admin::list_comments),
        )
        .route(
            "/api/admin/comments/{id}",
            axum::routing::get(admin::get_comment),
        )
        .route(
            "/api/admin/comments/{id}/urls",
            axum::routing::get(admin::comment_urls),
        )
        .route(
            "/api/admin/urls/lookup",
            axum::routing::get(admin::url_lookup),
        )
        .route(
            "/api/admin/authors/lookup",
            axum::routing::get(admin::author_lookup),
        )
        .route(
            "/api/admin/comments/context",
            axum::routing::post(admin::bulk_context),
        )
        .route(
            "/api/admin/moderate",
            axum::routing::post(admin::moderate).layer(GovernorLayer {
                config: Arc::new(admin_moderate_governor),
            }),
        )
        .route(
            "/api/admin/moderate/batch",
            axum::routing::post(admin::moderate_batch),
        )
        .route("/api/admin/export", axum::routing::get(admin::export))
        .route(
            "/api/admin/reactions",
            axum::routing::get(admin::list_reactions),
        )
        .route(
            "/api/admin/reactions/moderate",
            axum::routing::post(admin::moderate_reaction),
        )
        .route(
            "/api/admin/reactions/moderate/batch",
            axum::routing::post(admin::moderate_reactions_batch),
        )
        .route(
            "/api/admin/import",
            axum::routing::post(admin::import).layer(axum::extract::DefaultBodyLimit::max(
                admin::MAX_IMPORT_BODY_BYTES,
            )),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin::admin_auth,
        ));

    let swagger = utoipa_swagger_ui::SwaggerUi::new("/swagger-ui")
        .url("/api-docs/openapi.json", ApiDoc::openapi());

    let router = Router::new()
        .merge(swagger)
        .route("/healthz", axum::routing::get(healthz))
        .route("/api/version", axum::routing::get(version))
        .route("/admin", axum::routing::get(admin_dashboard))
        .route("/embed/comments.js", axum::routing::get(comments_js))
        .route("/api/admin/login", axum::routing::post(admin::login))
        .route("/api/admin/logout", axum::routing::post(admin::logout))
        .route(
            "/api/comment",
            axum::routing::post(comment_post::create_comment)
                .layer::<_, std::convert::Infallible>(body_limit)
                .layer(GovernorLayer {
                    config: native_governor.clone(),
                }),
        )
        .route(
            "/api/comment/{id}/delete",
            axum::routing::post(comment_post::delete_comment)
                .layer::<_, std::convert::Infallible>(body_limit)
                .layer(GovernorLayer {
                    config: native_governor.clone(),
                }),
        )
        .route(
            "/api/comment/{id}/reaction",
            axum::routing::post(reactions::add_reaction).layer(GovernorLayer {
                config: native_governor.clone(),
            }),
        )
        .route(
            "/api/comment/{id}/reaction",
            axum::routing::delete(reactions::remove_reaction).layer(GovernorLayer {
                config: native_governor,
            }),
        )
        .route(
            "/api/comments",
            axum::routing::get(comments_read::list_comments).layer(GovernorLayer {
                config: read_governor.clone(),
            }),
        )
        .route(
            "/feed.xml",
            axum::routing::get(feed::feed).layer(GovernorLayer {
                config: read_governor,
            }),
        );

    #[cfg(feature = "webmentions")]
    let router = router.route(
        "/api/webmention",
        axum::routing::post(webmention_post::receive_webmention)
            .layer::<_, std::convert::Infallible>(body_limit)
            .layer(GovernorLayer {
                config: Arc::new(webmention_governor),
            }),
    );

    // CORS intentionally applies to the PUBLIC router only. Admin routes are
    // server-side (same-origin dashboard); advertising them in preflight
    // would contradict SPEC §6.7.
    router.layer(cors).merge(admin_routes).with_state(state)
}

async fn admin_dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../../embed/admin.html"))
}

async fn comments_js() -> (
    axum::http::StatusCode,
    [(&'static str, &'static str); 2],
    &'static str,
) {
    (
        axum::http::StatusCode::OK,
        [
            ("content-type", "application/javascript"),
            ("cache-control", "public, max-age=3600"),
        ],
        include_str!("../../embed/comments.js"),
    )
}

#[utoipa::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Server is healthy", body = String),
    ),
)]
async fn healthz(State(state): State<AppState>) -> impl IntoResponse {
    // Readiness, not just liveness: the orchestrator must see failures when
    // the database stops answering (full disk, corruption, lost volume).
    // The probe is bounded so a wedged pool fails fast instead of hanging
    // the healthcheck past its timeout.
    let pool = state.pool.clone();
    let probed = tokio::time::timeout(std::time::Duration::from_secs(2), async move {
        tokio::task::spawn_blocking(move || {
            pool.get()
                .ok()
                .and_then(|conn| {
                    conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                        .ok()
                })
                .map(|v| v == 1)
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if probed {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    }
}

/// Reported binary and data-format versions. All values are compile-time
/// constants from their single source of truth — no manual sync needed.
#[derive(serde::Serialize)]
struct VersionInfo {
    version: &'static str,
    schema_version: i64,
    export_version: i64,
}

#[utoipa::path(
    get,
    path = "/api/version",
    responses(
        (status = 200, description = "Binary and data-format versions"),
    ),
)]
async fn version() -> axum::Json<VersionInfo> {
    axum::Json(VersionInfo {
        version: crate::APP_VERSION,
        schema_version: crate::db::pool::LATEST_SCHEMA_VERSION,
        export_version: crate::http::admin::data::EXPORT_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request, header};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    use crate::http::test_support::helpers;
    use crate::state::AppState;

    fn test_state() -> (AppState, tempfile::TempDir) {
        helpers::test_state()
    }

    fn request(method: axum::http::Method, uri: &str) -> Request<Body> {
        helpers::request(method, uri)
    }

    fn request_with_origin(method: axum::http::Method, uri: &str, origin: &str) -> Request<Body> {
        let mut req = helpers::request(method, uri);
        req.headers_mut()
            .insert(header::ORIGIN, origin.parse().unwrap());
        req
    }

    fn request_with_body(method: axum::http::Method, uri: &str, body: Vec<u8>) -> Request<Body> {
        let len = body.len();
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::CONTENT_LENGTH, len)
            .extension(axum::extract::ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                54321,
            ))))
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn healthz_returns_200() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request(axum::http::Method::GET, "/healthz"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn healthz_body_is_ok_on_healthy_db() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request(axum::http::Method::GET, "/healthz"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(body.as_ref(), b"ok", "healthy body stays compatible");
    }

    #[tokio::test]
    async fn healthz_returns_503_when_db_unreachable() {
        let (mut state, _dir) = test_state();
        // A pool whose file can never open: every probe fails. A short
        // connection timeout keeps the failure fast — r2d2 would otherwise
        // retry for its 30 s default (and the orphaned probe would hold
        // runtime teardown until it errors).
        let manager = r2d2_sqlite::SqliteConnectionManager::file(
            "/nonexistent-dir-zapiska-healthz/comments.db",
        );
        state.pool = r2d2::Pool::builder()
            .min_idle(Some(0))
            .connection_timeout(std::time::Duration::from_millis(200))
            .build(manager)
            .unwrap();
        let app = build_app(state);
        let resp = app
            .oneshot(request(axum::http::Method::GET, "/healthz"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            503,
            "healthz must fail when the database does not answer"
        );
    }

    #[tokio::test]
    async fn healthz_exhausted_pool_trips_probe_bound() {
        let (state, _dir) = test_state();
        // Check out every connection (r2d2 default max_size is 10) so the
        // probe cannot get one: it must hit the 2 s probe bound and answer
        // 503, not wait out the pool's 30 s connection timeout.
        let _held: Vec<_> = (0..10).map(|_| state.pool.get().unwrap()).collect();
        let app = build_app(state);
        let start = std::time::Instant::now();
        let resp = app
            .oneshot(request(axum::http::Method::GET, "/healthz"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 503, "exhausted pool reads as unhealthy");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "probe must be bounded, not wait out the pool timeout: {:?}",
            start.elapsed()
        );
    }
    #[tokio::test]
    async fn version_reports_single_sourced_versions() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request(axum::http::Method::GET, "/api/version"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            body["schema_version"],
            crate::db::pool::LATEST_SCHEMA_VERSION
        );
        assert_eq!(
            body["export_version"],
            crate::http::admin::data::EXPORT_VERSION
        );
    }

    #[tokio::test]
    async fn openapi_version_matches_package_version() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request(axum::http::Method::GET, "/api-docs/openapi.json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["info"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            body["paths"].get("/api/version").is_some(),
            "version endpoint must appear in OpenAPI"
        );
    }

    #[tokio::test]
    async fn body_limit_rejects_oversized_payload() {
        let valid_body = "target_path=/x&author_name=Alice&content=hello";
        let over_body = "target_path=/x&author_name=Alice&content=".to_string() + &"x".repeat(2000);

        let (state_base, _dir) = test_state();
        let mut state = state_base;
        state.config.max_body_size = valid_body.len() + 10;
        let app = build_app(state);

        // Under limit — should pass body limit and reach handler
        let under = valid_body.as_bytes().to_vec();
        let resp = app
            .clone()
            .oneshot(request_with_body(
                axum::http::Method::POST,
                "/api/comment",
                under,
            ))
            .await
            .unwrap();
        // Handler returns 201 for valid input, but body limit has passed.
        assert_eq!(resp.status(), 201);

        // Over limit — body limit should reject
        let over = over_body.as_bytes().to_vec();
        let resp = app
            .oneshot(request_with_body(
                axum::http::Method::POST,
                "/api/comment",
                over,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn cors_allows_configured_origin() {
        let (state, _dir) = test_state();
        let app = build_app(state);

        // Allowed origin: https://nithitsuki.com
        let resp = app
            .clone()
            .oneshot(request_with_origin(
                axum::http::Method::GET,
                "/api/comments",
                "https://nithitsuki.com",
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://nithitsuki.com"))
        );

        // Disallowed origin: http://evil.com
        let resp = app
            .oneshot(request_with_origin(
                axum::http::Method::GET,
                "/api/comments",
                "http://evil.com",
            ))
            .await
            .unwrap();
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    #[tokio::test]
    async fn cors_preflight_has_cors_headers() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request_with_origin(
                axum::http::Method::OPTIONS,
                "/api/comments",
                "https://nithitsuki.com",
            ))
            .await
            .unwrap();
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .is_some(),
            "preflight response should have Allow-Methods header"
        );
        assert_eq!(
            resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://nithitsuki.com"))
        );
    }

    #[tokio::test]
    async fn cors_does_not_advertise_admin_methods() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request_with_origin(
                axum::http::Method::OPTIONS,
                "/api/comments",
                "https://nithitsuki.com",
            ))
            .await
            .unwrap();
        let methods = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_METHODS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(methods.contains("GET"));
        assert!(methods.contains("POST"));
    }

    #[tokio::test]
    async fn rate_limit_native_endpoint_returns_429_after_burst() {
        let (state, _dir) = test_state();
        let tight_config = {
            use tower_governor::governor::GovernorConfigBuilder;
            use tower_governor::key_extractor::PeerIpKeyExtractor;

            GovernorConfigBuilder::default()
                .per_second(60)
                .burst_size(2)
                .key_extractor(PeerIpKeyExtractor)
                .finish()
                .expect("valid")
        };

        let app = Router::new()
            .route(
                "/api/comment",
                axum::routing::post(|| async {
                    (axum::http::StatusCode::NOT_IMPLEMENTED, "not implemented")
                })
                .layer(GovernorLayer {
                    config: Arc::new(tight_config),
                }),
            )
            .with_state(state);

        // Send 2 requests within burst.
        let resp = app
            .clone()
            .oneshot(request(axum::http::Method::POST, "/api/comment"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 501);

        let resp = app
            .clone()
            .oneshot(request(axum::http::Method::POST, "/api/comment"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 501);

        // 3rd request should exceed burst and return 429.
        let resp = app
            .oneshot(request(axum::http::Method::POST, "/api/comment"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 429);
    }

    // ── GET /api/comments tests ─────────────────────────────

    async fn seed_comment(state: &AppState, path: &str, author: &str, status: &str) {
        let repo = &state.repo;
        let id = repo
            .insert_comment(crate::db::repo::NewComment {
                target_path: path.to_string(),
                comment_type: "native".to_string(),
                source_url: None,
                author_name: author.to_string(),
                author_url: None,
                author_avatar: None,
                content: format!("comment by {author}"),
                parent_id: None,
                depth: 0,
                honeypot: false,
                delete_token: None,
                submitter_ip: None,
                submitter_ip_hash: None,
                content_hash: None,
            })
            .await
            .unwrap();
        if status != "pending" {
            repo.update_status(id, status).await.unwrap();
        }
    }

    fn request_uri(uri: &str) -> Request<Body> {
        request(axum::http::Method::GET, uri)
    }

    #[tokio::test]
    async fn read_returns_only_approved() {
        let (state, _dir) = test_state();
        seed_comment(&state, "/page", "Alice", "approved").await;
        seed_comment(&state, "/page", "Bob", "pending").await;
        seed_comment(&state, "/page", "Charlie", "spam").await;
        seed_comment(&state, "/page", "Diana", "deleted").await;

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/page"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1, "only one approved comment returned");
        assert_eq!(comments[0]["author_name"], "Alice");
        assert_eq!(body["total"], 1);
    }

    #[tokio::test]
    async fn read_path_missing_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app.oneshot(request_uri("/api/comments")).await.unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn read_path_invalid_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);

        // Path without leading slash.
        let resp = app
            .clone()
            .oneshot(request_uri("/api/comments?path=no-slash"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Path with double slash.
        let resp = app
            .clone()
            .oneshot(request_uri("/api/comments?path=//bad"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn read_limit_defaults_to_50() {
        let (state, _dir) = test_state();
        // Insert 51 approved comments.
        for i in 0..51 {
            let name = format!("User{i}");
            seed_comment(&state, "/many", &name, "approved").await;
        }

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/many"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 50, "default limit is 50");
    }

    #[tokio::test]
    async fn read_limit_clamped_to_100() {
        let (state, _dir) = test_state();
        for i in 0..120 {
            let name = format!("User{i}");
            seed_comment(&state, "/clamp", &name, "approved").await;
        }

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/clamp&limit=200"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 100, "limit clamped to 100");
    }

    #[tokio::test]
    async fn read_before_cursor_paginates() {
        let (state, _dir) = test_state();
        // Insert 5 approved comments, ids 1..=5.
        for i in 0..5 {
            seed_comment(&state, "/paged", &format!("U{i}"), "approved").await;
        }
        // IDs: 1, 2, 3, 4, 5. DESC order: 5, 4, 3, 2, 1.

        let app = build_app(state);

        // First page: newest 2.
        let resp = app
            .clone()
            .oneshot(request_uri("/api/comments?path=/paged&limit=2"))
            .await
            .unwrap();
        let body1: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let page1 = body1["comments"].as_array().unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0]["author_name"], "U4"); // id 5
        assert_eq!(page1[1]["author_name"], "U3"); // id 4
        let last_id = page1[1]["id"].as_i64().unwrap();

        // Second page: before = last_id.
        let resp2 = app
            .clone()
            .oneshot(request_uri(&format!(
                "/api/comments?path=/paged&limit=2&before={last_id}"
            )))
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let page2 = body2["comments"].as_array().unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0]["author_name"], "U2"); // id 3
        assert_eq!(page2[1]["author_name"], "U1"); // id 2
    }

    #[tokio::test]
    async fn read_sort_oldest_returns_ascending() {
        let (state, _dir) = test_state();
        for i in 0..5 {
            seed_comment(&state, "/sorted", &format!("U{i}"), "approved").await;
        }
        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/sorted&sort=oldest"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let comments = body["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 5);
        let ids: Vec<i64> = comments.iter().map(|c| c["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5], "oldest first, ascending");
        assert_eq!(comments[0]["author_name"], "U0");
    }

    #[tokio::test]
    async fn read_sort_oldest_with_after_cursor_paginates() {
        let (state, _dir) = test_state();
        for i in 0..5 {
            seed_comment(&state, "/sorted-cur", &format!("U{i}"), "approved").await;
        }
        let app = build_app(state);

        let resp = app
            .clone()
            .oneshot(request_uri(
                "/api/comments?path=/sorted-cur&sort=oldest&limit=2",
            ))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let page1 = body["comments"].as_array().unwrap();
        assert_eq!(page1[0]["author_name"], "U0");
        assert_eq!(page1[1]["author_name"], "U1");
        let last_id = page1[1]["id"].as_i64().unwrap();

        let resp2 = app
            .clone()
            .oneshot(request_uri(&format!(
                "/api/comments?path=/sorted-cur&sort=oldest&limit=2&after={last_id}"
            )))
            .await
            .unwrap();
        let body2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let page2 = body2["comments"].as_array().unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0]["author_name"], "U2");
        assert_eq!(page2[1]["author_name"], "U3");
    }

    #[tokio::test]
    async fn read_sort_newest_is_explicit_and_default() {
        let (state, _dir) = test_state();
        for i in 0..3 {
            seed_comment(&state, "/sorted-new", &format!("U{i}"), "approved").await;
        }
        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/sorted-new&sort=newest"))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let ids: Vec<i64> = body["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 2, 1], "newest first");
    }

    #[tokio::test]
    async fn read_sort_invalid_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/x&sort=by-likes"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "unknown sort must be rejected");
    }

    #[tokio::test]
    async fn read_before_cursor_returns_empty_when_none_older() {
        let (state, _dir) = test_state();
        seed_comment(&state, "/empty-cur", "A", "approved").await;

        let app = build_app(state);
        // before = 1 means id < 1, which is empty.
        let resp = app
            .oneshot(request_uri("/api/comments?path=/empty-cur&before=1"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["comments"].as_array().unwrap().len(), 0);
        assert_eq!(body["total"], 1);
    }

    #[tokio::test]
    async fn read_json_shape_matches_spec() {
        let (state, _dir) = test_state();
        seed_comment(&state, "/shape", "Alice", "approved").await;

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/shape"))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();

        // Top-level keys.
        assert!(body.get("total").is_some(), "response has 'total'");
        assert!(body.get("comments").is_some(), "response has 'comments'");

        let c = &body["comments"][0];
        assert!(c.get("id").is_some(), "comment has 'id'");
        assert!(
            c.get("comment_type").is_some(),
            "comment has 'comment_type'"
        );
        assert!(c.get("author_name").is_some(), "comment has 'author_name'");
        assert!(c.get("content").is_some(), "comment has 'content'");
        assert!(c.get("created_at").is_some(), "comment has 'created_at'");

        // Internal fields should NOT leak.
        assert!(
            c.get("source_url").is_none(),
            "source_url must NOT be in response"
        );
        assert!(c.get("status").is_none(), "status must NOT be in response");
        assert!(
            c.get("updated_at").is_none(),
            "updated_at must NOT be in response"
        );
        assert!(
            c.get("target_path").is_none(),
            "target_path must NOT be in response"
        );
    }

    #[tokio::test]
    async fn read_approved_excludes_non_approved() {
        // Ensure that non-approved comments for the SAME path are never returned.
        let (state, _dir) = test_state();
        seed_comment(&state, "/x", "Approved", "approved").await;
        seed_comment(&state, "/x", "Pend", "pending").await;
        seed_comment(&state, "/x", "Spammy", "spam").await;
        seed_comment(&state, "/x", "Deleted", "deleted").await;

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/api/comments?path=/x"))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();

        let authors: Vec<&str> = body["comments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["author_name"].as_str().unwrap())
            .collect();
        assert_eq!(authors, vec!["Approved"]);
    }

    // ── RSS feed tests ───────────────────────────────────────

    /// Parse the response body as RSS XML; returns the item titles.
    fn feed_item_titles(body: &[u8]) -> Vec<String> {
        use quick_xml::Reader;
        use quick_xml::events::Event;
        let mut reader = Reader::from_reader(body);
        let mut titles = Vec::new();
        let mut buf = Vec::new();
        let mut in_item = false;
        let mut in_title = false;
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => match e.name().as_ref() {
                    b"item" => in_item = true,
                    b"title" if in_item => in_title = true,
                    _ => {}
                },
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"item" => in_item = false,
                    b"title" if in_item => in_title = false,
                    _ => {}
                },
                Ok(Event::Text(t)) if in_title => {
                    titles.push(t.unescape().unwrap_or_default().into_owned());
                }
                Ok(Event::Eof) => break,
                Err(e) => panic!("feed is not well-formed XML: {e}"),
                _ => {}
            }
            buf.clear();
        }
        titles
    }

    #[tokio::test]
    async fn feed_returns_only_approved_comments() {
        let (state, _dir) = test_state();
        seed_comment(&state, "/feed-test", "Alice", "approved").await;
        seed_comment(&state, "/feed-test", "Bob", "pending").await;
        seed_comment(&state, "/feed-test", "Charlie", "spam").await;

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/feed.xml?path=/feed-test"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/rss+xml; charset=utf-8"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let titles = feed_item_titles(&body);
        assert_eq!(titles, vec!["Alice"], "only approved comments in feed");
    }

    #[tokio::test]
    async fn feed_global_includes_all_paths_with_path_in_titles() {
        let (state, _dir) = test_state();
        seed_comment(&state, "/blog/a", "Alice", "approved").await;
        seed_comment(&state, "/blog/b", "Bob", "approved").await;

        let app = build_app(state);
        let resp = app.oneshot(request_uri("/feed.xml")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let titles = feed_item_titles(&body);
        assert_eq!(titles, vec!["Bob on /blog/b", "Alice on /blog/a"]);
    }

    #[tokio::test]
    async fn feed_per_path_titles_are_bare_author_names() {
        let (state, _dir) = test_state();
        seed_comment(&state, "/blog/a", "Alice", "approved").await;

        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/feed.xml?path=/blog/a"))
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let titles = feed_item_titles(&body);
        assert_eq!(titles, vec!["Alice"]);
    }

    #[tokio::test]
    async fn feed_invalid_path_returns_400() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/feed.xml?path=no-slash"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn feed_empty_is_still_well_formed() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app.oneshot(request_uri("/feed.xml")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let xml = String::from_utf8(body.to_vec()).unwrap();
        assert!(xml.contains("<channel>"), "empty feed has a channel");
        assert!(
            xml.contains("<lastBuildDate>"),
            "empty feed has lastBuildDate"
        );
        assert!(feed_item_titles(&body).is_empty());
    }

    #[tokio::test]
    async fn feed_default_limit_is_50() {
        let (state, _dir) = test_state();
        for i in 0..55 {
            seed_comment(&state, "/many", &format!("User{i}"), "approved").await;
        }
        let app = build_app(state);
        let resp = app
            .oneshot(request_uri("/feed.xml?path=/many"))
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            feed_item_titles(&body).len(),
            50,
            "default feed limit is 50"
        );
    }

    #[tokio::test]
    async fn cors_does_not_apply_to_admin_routes() {
        let (state, _dir) = test_state();
        let app = build_app(state);
        let resp = app
            .oneshot(request_with_origin(
                axum::http::Method::OPTIONS,
                "/api/admin/pending",
                "https://nithitsuki.com",
            ))
            .await
            .unwrap();
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "admin routes must not advertise CORS (SPEC §6.7)"
        );
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_METHODS)
                .is_none(),
            "admin routes must not answer preflight"
        );
    }
}
