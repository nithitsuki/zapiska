#[cfg(feature = "webmentions")]
pub mod avatar;
/// Crate version from Cargo.toml — the single source of truth for the
/// reported application version (used by `--version`, `/api/version`,
/// and the OpenAPI document).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub mod config;
pub mod db;
pub mod error;
#[cfg(feature = "webmentions")]
pub mod fetch;
pub mod github;
pub mod http;
pub mod ip_hash;
pub mod language;
#[cfg(feature = "webmentions")]
pub mod mf2;
pub mod moderation;
pub mod notify;
pub mod openapi;
pub mod sanitize;
#[cfg(feature = "webmentions")]
pub mod ssrf;
pub mod state;
pub mod timeutil;
pub mod turnstile;
pub mod validate;
#[cfg(feature = "webmentions")]
pub mod worker;
