use axum::body::Body;
use axum::http::{HeaderValue, Response, StatusCode, header};
use governor::middleware::NoOpMiddleware;
use std::time::Duration;
use tower_governor::governor::{GovernorConfig, GovernorConfigBuilder};
use tower_governor::{GovernorError, GovernorLayer};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::Config;
use crate::http::peer::ClientIdentityExtractor;

/// Concrete governor config type used throughout: keyed by the normalized
/// [`crate::http::peer::ClientIdentity`], so governors, the `Limiter`, and
/// IP hashing always agree on who the client is.
pub type RateLimitConfig = GovernorConfig<ClientIdentityExtractor, NoOpMiddleware>;

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

/// Sustained request rate (requests/second) a `(burst, window)` pair enforces
/// once the burst bucket is spent: `burst` requests per `window` seconds.
/// This is the number the docs rate tables render next to each default.
pub fn sustained_rate_per_sec(burst: u32, window_secs: u64) -> f64 {
    burst as f64 / window_secs.max(1) as f64
}

/// Per-cell GCRA replenish interval implementing a `(burst, window)` pair.
///
/// `tower_governor`'s `per_second(n)` primitive means "one cell back per `n`
/// seconds", so a bare `per_second(window)` would enforce a sustained rate of
/// 1 request per window — far below the "burst requests per window" the
/// `RATE_LIMIT_*` knob names promise. Dividing the window by the burst keeps
/// the burst bucket at `burst` cells while the sustained rate matches the
/// documented `burst / window` (see [`sustained_rate_per_sec`]).
pub fn governor_period(burst: u32, window_secs: u64) -> Duration {
    const NANOS_PER_SEC: u64 = 1_000_000_000;
    let burst = u64::from(burst.max(1));
    let window_secs = window_secs.max(1);
    Duration::from_nanos(window_secs.saturating_mul(NANOS_PER_SEC) / burst)
}

/// Governor-side 429s use the documented JSON error shape
/// (`{"error", "code": "rate_limited"}`, docs/api.md "Error response") with a
/// `Retry-After` hint, matching the handler-side [`crate::error::AppError`]
/// 429s — not tower_governor's default plain-text "Too Many Requests".
fn json_rate_limit_error(err: GovernorError) -> Response<Body> {
    match err {
        GovernorError::TooManyRequests { wait_time, headers } => {
            let body = serde_json::json!({
                "error": format!("rate limited, retry after {wait_time}s"),
                "code": "rate_limited",
            })
            .to_string();
            let mut resp = Response::new(Body::from(body));
            *resp.status_mut() = StatusCode::TOO_MANY_REQUESTS;
            // Preserve tower's rate-limit hints (x-ratelimit-after,
            // retry-after); the JSON shape and Retry-After below land on top.
            if let Some(headers) = headers {
                resp.headers_mut().extend(headers);
            }
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            resp.headers_mut().insert(
                header::RETRY_AFTER,
                wait_time
                    .to_string()
                    .parse()
                    .expect("u64 retry-after is a valid header value"),
            );
            resp
        }
        mut other => other.as_response(),
    }
}

/// Wrap a finished governor config in its tower layer. One helper so the
/// struct-literal shape lives next to the config constructors, not at every
/// route in `routes.rs`.
pub fn governor_layer(
    config: std::sync::Arc<RateLimitConfig>,
) -> GovernorLayer<ClientIdentityExtractor, NoOpMiddleware> {
    GovernorLayer { config }
}

fn governor_config(burst: u32, window_secs: u64, trust_proxy: bool) -> RateLimitConfig {
    // Sustained-rate sanity: a degenerate pair must not silently widen the
    // gate (finish() already rejects zero burst/period; this names why).
    debug_assert!(sustained_rate_per_sec(burst, window_secs) > 0.0);
    let mut builder = GovernorConfigBuilder::default();
    builder
        .period(governor_period(burst, window_secs))
        .burst_size(burst);
    let mut builder = builder.key_extractor(ClientIdentityExtractor::new(trust_proxy));
    builder.error_handler(json_rate_limit_error);
    builder.finish().expect("valid governor config")
}

pub fn native_comment_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_native_burst,
        config.rate_limit_native_window_secs,
        config.trust_proxy,
    )
}

#[cfg(feature = "webmentions")]
pub fn webmention_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_webmention_burst,
        config.rate_limit_webmention_window_secs,
        config.trust_proxy,
    )
}

pub fn read_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_read_burst,
        config.rate_limit_read_window_secs,
        config.trust_proxy,
    )
}

pub fn admin_moderate_governor(config: &Config) -> RateLimitConfig {
    governor_config(
        config.rate_limit_admin_moderate_burst,
        config.rate_limit_admin_moderate_window_secs,
        config.trust_proxy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Machine-readable anchor embedded in the docs rate tables
    /// (docs/architecture.md, docs/security.md):
    /// `<!-- RATE-LIMITS: native=100/60 webmention=60/60 ... -->`.
    /// The drift test below fails when code defaults and docs disagree in
    /// either direction.
    const RATE_LIMITS_ANCHOR: &str = "RATE-LIMITS:";

    fn anchor_pairs(doc: &str) -> Vec<(&str, u32, u64)> {
        let line = doc
            .lines()
            .find(|l| l.contains(RATE_LIMITS_ANCHOR))
            .expect("docs must embed a RATE-LIMITS anchor line");
        line.split_whitespace()
            .filter_map(|tok| {
                let (name, rest) = tok.split_once('=')?;
                let (burst, window) = rest.split_once('/')?;
                if name.ends_with(':') || burst.is_empty() || window.is_empty() {
                    return None;
                }
                let name = name.trim_end_matches(':');
                if !name.chars().all(|c| c.is_ascii_alphanumeric()) {
                    return None;
                }
                Some((name, burst.parse().ok()?, window.parse().ok()?))
            })
            .collect()
    }

    #[test]
    fn governor_period_implements_burst_per_window() {
        // Effective GCRA behavior, not a config echo: these Durations are the
        // actual per-cell replenish intervals fed to the rate limiter.
        assert_eq!(governor_period(100, 60), Duration::from_millis(600));
        assert_eq!(governor_period(60, 60), Duration::from_secs(1));
        assert_eq!(governor_period(300, 60), Duration::from_millis(200));
        assert_eq!(governor_period(30, 60), Duration::from_secs(2));
        // Tight test budgets stay expressible below one second.
        assert_eq!(governor_period(2, 1), Duration::from_millis(500));
        // Degenerate inputs never produce a zero period (finish() rejects it).
        assert!(!governor_period(0, 0).is_zero());
    }

    #[test]
    fn sustained_rates_match_documented_values() {
        assert!((sustained_rate_per_sec(100, 60) - 100.0 / 60.0).abs() < f64::EPSILON);
        assert_eq!(sustained_rate_per_sec(60, 60), 1.0);
        assert_eq!(sustained_rate_per_sec(300, 60), 5.0);
        assert_eq!(sustained_rate_per_sec(30, 60), 0.5);
    }

    #[test]
    fn docs_rate_tables_match_effective_defaults() {
        let defaults = Config::default();
        let expected = [
            (
                "native",
                defaults.rate_limit_native_burst,
                defaults.rate_limit_native_window_secs,
            ),
            (
                "webmention",
                defaults.rate_limit_webmention_burst,
                defaults.rate_limit_webmention_window_secs,
            ),
            (
                "read",
                defaults.rate_limit_read_burst,
                defaults.rate_limit_read_window_secs,
            ),
            (
                "admin",
                defaults.rate_limit_admin_moderate_burst,
                defaults.rate_limit_admin_moderate_window_secs,
            ),
        ];
        for doc in [
            include_str!("../../docs/architecture.md"),
            include_str!("../../docs/security.md"),
        ] {
            let pairs = anchor_pairs(doc);
            for (name, burst, window) in &expected {
                let found = pairs
                    .iter()
                    .find(|(n, _, _)| n == name)
                    .unwrap_or_else(|| panic!("docs anchor missing '{name}' pair"));
                assert_eq!(
                    (found.1, found.2),
                    (*burst, *window),
                    "docs anchor drifted from Config::default for '{name}'"
                );
                // The documented sustained rate must equal the rate the
                // governor actually enforces — a stale table column fails here.
                let sustained = format!("{:.2}/s", sustained_rate_per_sec(*burst, *window));
                assert!(
                    doc.contains(&sustained),
                    "docs must render the effective sustained rate '{sustained}' for '{name}'"
                );
            }
        }
    }
}
