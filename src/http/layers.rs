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

    let origins: Vec<axum::http::HeaderValue> = config
        .allowed_cors_origin
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<axum::http::HeaderValue>()
                .expect("ALLOWED_CORS_ORIGIN validated at config load")
        })
        .collect();

    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |o, _| {
            origins.iter().any(|v| v == o)
        }))
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

pub fn native_comment_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_native_window_secs,
        config.rate_limit_native_burst,
    )
}

#[cfg(feature = "webmentions")]
pub fn webmention_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_webmention_window_secs,
        config.rate_limit_webmention_burst,
    )
}

pub fn read_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_read_window_secs,
        config.rate_limit_read_burst,
    )
}

pub fn admin_moderate_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_admin_moderate_window_secs,
        config.rate_limit_admin_moderate_burst,
    )
}
