use governor::middleware::NoOpMiddleware;
use std::time::Duration;
use tower_governor::governor::GovernorConfig;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::PeerIpKeyExtractor;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::Config;

/// Concrete governor config type used throughout.
pub type RateLimitConfig = GovernorConfig<PeerIpKeyExtractor, NoOpMiddleware>;

pub fn cors_layer(config: &Config) -> CorsLayer {
    if config.allowed_cors_origin == "*" {
        return CorsLayer::new()
            .allow_origin(AllowOrigin::any())
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::POST,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                axum::http::header::AUTHORIZATION,
            ]);
    }

    let origin = config
        .allowed_cors_origin
        .parse::<axum::http::HeaderValue>()
        .expect("ALLOWED_CORS_ORIGIN validated at config load");

    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |o, _| o == origin))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ])
        .max_age(Duration::from_secs(600))
}

pub fn body_limit_layer(config: &Config) -> RequestBodyLimitLayer {
    RequestBodyLimitLayer::new(config.max_body_size)
}

fn governor_config(per_seconds: u64, burst: u32) -> RateLimitConfig {
    GovernorConfigBuilder::default()
        .per_second(per_seconds)
        .burst_size(burst)
        .key_extractor(PeerIpKeyExtractor)
        .finish()
        .expect("valid governor config")
}

pub fn native_comment_governor() -> RateLimitConfig {
    // Allow 50 per 60s for development / e2e testing.
    // In production this should be lower (5 per 60s).
    governor_config(60, 50)
}

#[cfg(feature = "webmentions")]
pub fn webmention_governor() -> RateLimitConfig {
    governor_config(60, 30) // 30 per 60s
}

pub fn read_governor() -> RateLimitConfig {
    governor_config(60, 60)   // 60 per 60s
}

pub fn admin_moderate_governor() -> RateLimitConfig {
    governor_config(60, 10)   // 10 per 60s, slows brute-force
}
