//! Route-group constructors: the layer recipe lives once here.
//!
//! Each group bundles its routes with their governor and body-limit layers in
//! the one correct order (body limit inside, governor outside), so adding a
//! route to a group means one line here instead of replicating the
//! `.layer(body).layer(governor)` pair — and its `Infallible` turbofish — at
//! a new definition site.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;

use crate::http::admin;
use crate::http::comment_post;
use crate::http::comments_read;
use crate::http::feed;
use crate::http::layers;
use crate::http::layers::RateLimitConfig;
#[cfg(feature = "webmentions")]
use crate::http::webmention_post;
use crate::state::AppState;

/// Native write group: comment submission, self-service deletion, and both
/// reaction methods. All four share one governor bucket (one client's burst
/// spans the whole group) and the form routes share the body limit.
pub fn native_write_routes(
    governor: Arc<RateLimitConfig>,
    body_limit: RequestBodyLimitLayer,
) -> Router<AppState> {
    Router::new()
        .route(
            "/api/comment",
            axum::routing::post(comment_post::create_comment)
                .layer::<_, Infallible>(body_limit)
                .layer(layers::governor_layer(governor.clone())),
        )
        .route(
            "/api/comment/{id}/delete",
            axum::routing::post(comment_post::delete_comment)
                .layer::<_, Infallible>(body_limit)
                .layer(layers::governor_layer(governor.clone())),
        )
        .route(
            "/api/comment/{id}/reaction",
            axum::routing::post(crate::http::reactions::add_reaction)
                .layer(layers::governor_layer(governor.clone())),
        )
        .route(
            "/api/comment/{id}/reaction",
            axum::routing::delete(crate::http::reactions::remove_reaction)
                .layer(layers::governor_layer(governor)),
        )
}

/// Public read group: the JSON read API and the RSS feed share one governor
/// bucket.
pub fn public_read_routes(governor: Arc<RateLimitConfig>) -> Router<AppState> {
    Router::new()
        .route(
            "/api/comments",
            axum::routing::get(comments_read::list_comments)
                .layer(layers::governor_layer(governor.clone())),
        )
        .route(
            "/feed.xml",
            axum::routing::get(feed::feed).layer(layers::governor_layer(governor)),
        )
}

/// Session group: login is throttled per IP on its own bucket (brute-force
/// oracle protection); logout needs no throttle.
pub fn session_routes(login_governor: Arc<RateLimitConfig>) -> Router<AppState> {
    Router::new()
        .route(
            "/api/admin/login",
            axum::routing::post(admin::login).layer(layers::governor_layer(login_governor)),
        )
        .route("/api/admin/logout", axum::routing::post(admin::logout))
}

/// Webmention ingress: one route with the form body limit and its own
/// governor bucket.
#[cfg(feature = "webmentions")]
pub fn webmention_routes(
    governor: Arc<RateLimitConfig>,
    body_limit: RequestBodyLimitLayer,
) -> Router<AppState> {
    Router::new().route(
        "/api/webmention",
        axum::routing::post(webmention_post::receive_webmention)
            .layer::<_, Infallible>(body_limit)
            .layer(layers::governor_layer(governor)),
    )
}

/// Per-route governor budgets for the protected admin group. Login, batch
/// moderation, single reaction moderation, reaction batch moderation, and
/// export each get their own instance of the same admin-moderation budget
/// (separate buckets, same configured values): brute-force and bulk-dump
/// abuse are throttled per IP without spending the single-moderation budget.
/// No new knobs — one pattern everywhere.
pub struct AdminGovernors {
    pub moderate: RateLimitConfig,
    pub batch: RateLimitConfig,
    pub export: RateLimitConfig,
    pub reactions: RateLimitConfig,
    pub reactions_batch: RateLimitConfig,
}

impl AdminGovernors {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            moderate: layers::admin_moderate_governor(config),
            batch: layers::admin_moderate_governor(config),
            export: layers::admin_moderate_governor(config),
            reactions: layers::admin_moderate_governor(config),
            reactions_batch: layers::admin_moderate_governor(config),
        }
    }
}

/// Every path served by [`protected_admin_routes`]. The no-CORS test
/// iterates this list, and asserts its length, so adding a protected route
/// without listing it here fails loudly instead of silently gaining CORS.
pub const ADMIN_ROUTE_PATHS: &[&str] = &[
    "/api/admin/pending",
    "/api/admin/paths",
    "/api/admin/comments",
    "/api/admin/comments/{id}",
    "/api/admin/comments/{id}/urls",
    "/api/admin/urls/lookup",
    "/api/admin/authors/lookup",
    "/api/admin/comments/context",
    "/api/admin/moderate",
    "/api/admin/moderate/batch",
    "/api/admin/export",
    "/api/admin/reactions",
    "/api/admin/reactions/moderate",
    "/api/admin/reactions/moderate/batch",
    "/api/admin/import",
];

/// Protected admin group: every route below requires the admin token (Bearer
/// or session cookie) via [`admin::admin_auth`]. Throttled routes draw on
/// their own [`AdminGovernors`] bucket.
pub fn protected_admin_routes(state: &AppState, governors: AdminGovernors) -> Router<AppState> {
    // Fail fast in debug builds when a route is added below without listing
    // it in ADMIN_ROUTE_PATHS — otherwise the no-CORS contract test would
    // silently miss the new path.
    debug_assert_eq!(
        ADMIN_ROUTE_PATHS.len(),
        15,
        "list every protected_admin_routes path in ADMIN_ROUTE_PATHS"
    );
    Router::new()
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
            axum::routing::post(admin::moderate)
                .layer(layers::governor_layer(Arc::new(governors.moderate))),
        )
        .route(
            "/api/admin/moderate/batch",
            axum::routing::post(admin::moderate_batch)
                .layer(layers::governor_layer(Arc::new(governors.batch))),
        )
        .route(
            "/api/admin/export",
            axum::routing::get(admin::export)
                .layer(layers::governor_layer(Arc::new(governors.export))),
        )
        .route(
            "/api/admin/reactions",
            axum::routing::get(admin::list_reactions),
        )
        .route(
            "/api/admin/reactions/moderate",
            axum::routing::post(admin::moderate_reaction)
                .layer(layers::governor_layer(Arc::new(governors.reactions))),
        )
        .route(
            "/api/admin/reactions/moderate/batch",
            axum::routing::post(admin::moderate_reactions_batch)
                .layer(layers::governor_layer(Arc::new(governors.reactions_batch))),
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
        ))
}

/// Final assembly: CORS wraps the public router only, and the protected
/// admin group merges after — so admin responses never advertise CORS. The
/// ordering invariant is this function boundary (callers cannot reorder a
/// comment-guarded line pair); see the `cors_does_not_apply_to_admin_routes`
/// and `every_protected_admin_route_lacks_cors` tests.
pub fn compose(
    public: Router<AppState>,
    cors: CorsLayer,
    admin: Router<AppState>,
) -> Router<AppState> {
    public.layer(cors).merge(admin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    use crate::http::test_support::helpers;

    #[tokio::test]
    async fn admin_path_list_covers_all_protected_routes() {
        // Fails closed when a protected route is added without listing it.
        assert_eq!(
            ADMIN_ROUTE_PATHS.len(),
            15,
            "keep ADMIN_ROUTE_PATHS in sync with protected_admin_routes"
        );
    }

    #[tokio::test]
    async fn native_write_group_returns_json_429_from_real_composition() {
        // The group constructor (not a hand-rolled stack) with a tight
        // governor trips 429 in the documented JSON shape.
        let (mut state, _dir) = helpers::test_state();
        state.config.rate_limit_native_burst = 2;
        state.config.rate_limit_native_window_secs = 60;
        let governor = Arc::new(layers::native_comment_governor(&state.config));
        let app = native_write_routes(governor, layers::body_limit_layer(&state.config))
            .with_state(state);
        let body = "target_path=/x&author_name=Alice&content=hello";
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(helpers::form_request("/api/comment", body))
                .await
                .unwrap();
            assert_eq!(resp.status(), 201);
        }
        let resp = app
            .oneshot(helpers::form_request("/api/comment", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), 429);
        let json: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1024).await.unwrap())
                .expect("group 429 body must be JSON");
        assert_eq!(json["code"], "rate_limited");
    }

    #[tokio::test]
    async fn read_group_shares_one_bucket() {
        // Both read routes draw on the same governor instance: exhausting the
        // burst via the JSON API also throttles the feed.
        let (mut state, _dir) = helpers::test_state();
        state.config.rate_limit_read_burst = 1;
        state.config.rate_limit_read_window_secs = 60;
        let governor = Arc::new(layers::read_governor(&state.config));
        let app = public_read_routes(governor).with_state(state);
        let resp = app
            .clone()
            .oneshot(helpers::request(
                axum::http::Method::GET,
                "/api/comments?path=/x",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let resp = app
            .oneshot(helpers::request(axum::http::Method::GET, "/feed.xml"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            429,
            "read group shares one bucket across its routes"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("group 429 body must be JSON");
        assert_eq!(json["code"], "rate_limited");
    }
}
