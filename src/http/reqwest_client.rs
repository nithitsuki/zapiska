use std::str::FromStr;
use std::time::Duration;

use reqwest::Client;

use crate::config::Config;
use crate::ssrf::{SsrfError, resolve_and_check, ssrf_safe_redirect_policy};

/// Build an SSRF-safe reqwest Client that enforces the private-IP blocklist
/// at every redirect hop. Allows both HTTP and HTTPS — SSRF safety comes from
/// DNS resolution checks + redirect policy, not from https_only.
pub fn build_client(config: &Config) -> Client {
    Client::builder()
        .user_agent(format!(
            "webmention.nithitsuki.com/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_millis(config.fetch_timeout_ms))
        .connect_timeout(Duration::from_secs(10))
        .redirect(ssrf_safe_redirect_policy())
        .build()
        .expect("reqwest client build")
}

/// Fetch a URL through the SSRF-safe client, returning the response text.
/// Pre-fetches DNS and validates the host before connecting (belt + suspenders).
///
/// When `allow_loopback` is true, the SSRF check skips loopback addresses
/// (127.0.0.0/8, ::1). This is intended for integration tests using mock servers;
/// production callers should always pass `false`.
pub async fn fetch_url(
    client: &Client,
    url: &str,
    allow_loopback: bool,
) -> Result<String, FetchError> {
    let parsed = url::Url::parse(url).map_err(|e| FetchError::InvalidUrl {
        url: url.to_string(),
        source: e,
    })?;

    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::NoHost(url.to_string()))?;

    // SSRF check: skip loopback when allow_loopback is set (for test mocks).
    if !allow_loopback || !is_loopback(host) {
        resolve_and_check(host).await.map_err(FetchError::Ssrf)?;
    }

    let resp = client.get(url).send().await.map_err(|e| FetchError::Http {
        url: url.to_string(),
        source: e,
    })?;

    let status = resp.status();
    if status == reqwest::StatusCode::GONE {
        return Err(FetchError::Gone(url.to_string()));
    }

    if !status.is_success() {
        return Err(FetchError::HttpStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }

    resp.text().await.map_err(|e| FetchError::BodyRead {
        url: url.to_string(),
        source: e,
    })
}

#[derive(Debug)]
pub enum FetchError {
    InvalidUrl {
        url: String,
        source: url::ParseError,
    },
    NoHost(String),
    Ssrf(SsrfError),
    Http {
        url: String,
        source: reqwest::Error,
    },
    HttpStatus {
        url: String,
        status: u16,
    },
    Gone(String),
    BodyRead {
        url: String,
        source: reqwest::Error,
    },
}

/// Check if a host string is a loopback address (IPv4 127.x.x.x or IPv6 ::1).
fn is_loopback(host: &str) -> bool {
    if host == "::1" || host == "[::1]" {
        return true;
    }
    if let Ok(ip) = std::net::IpAddr::from_str(host)
        && ip.is_loopback()
    {
        return true;
    }
    false
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl { url, source } => {
                write!(f, "invalid URL {url}: {source}")
            }
            Self::NoHost(url) => write!(f, "no host in URL {url}"),
            Self::Ssrf(e) => write!(f, "SSRF blocked: {e}"),
            Self::Http { url, source } => write!(f, "HTTP error fetching {url}: {source}"),
            Self::HttpStatus { url, status } => {
                write!(f, "HTTP {status} fetching {url}")
            }
            Self::Gone(url) => write!(f, "source {url} is gone (410)"),
            Self::BodyRead { url, source } => {
                write!(f, "failed to read body from {url}: {source}")
            }
        }
    }
}

impl std::error::Error for FetchError {}
