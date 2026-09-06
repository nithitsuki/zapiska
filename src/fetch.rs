//! One guarded door for every outbound fetch of untrusted URLs.
//!
//! Placement: `src/fetch.rs` sits beside `ssrf.rs` (the blocklist), `mf2.rs`
//! (the parser), and `worker.rs` (the main caller) so the whole
//! fetch-check-parse seam is one top-level neighbourhood, not buried under
//! `http/` next to route handlers.
//!
//! [`SafeFetcher`] owns the only SSRF-safe way to turn an untrusted URL into
//! bytes: normalize the host, resolve-and-check it against the blocklist,
//! fetch with reqwest redirect-following **disabled**, and re-check **every**
//! redirect target with a fresh DNS resolution before following it. That
//! closes the hole the old synchronous redirect policy could not: redirect
//! targets that are hostnames (or trailing-dot forms like `http://localhost./`)
//! resolving to private addresses. Redirects are capped ([`DEFAULT_MAX_REDIRECTS`],
//! fail-closed) and bodies are capped ([`DEFAULT_MAX_BYTES`], enforced while
//! streaming so a huge body never materializes). The body is parsed exactly
//! once into [`FetchedDoc`].
//!
//! Callers must not fetch untrusted URLs with a bare `reqwest::Client` — the
//! old baked-in redirect-policy client promised safety by calling convention
//! and one production caller bypassed it entirely. This module is the only
//! public fetch API.
//!
//! Residual risk (honest limitation, not a second hole): check-then-connect
//! without address pinning cannot defeat DNS rebinding between the
//! `resolve_and_check` lookup and reqwest's own connect-time resolution.
//! `docs/security.md` states this explicitly.

use std::time::Duration;

use reqwest::Client;
use reqwest::header::{CONTENT_LENGTH, LOCATION};
use scraper::Html;
use url::Url;

use crate::ssrf::{is_loopback_host, resolve_and_check};

/// Default cap on followed redirects (fail-closed past this many hops).
pub const DEFAULT_MAX_REDIRECTS: usize = 5;
/// Default cap on response bodies (1 MiB — generous for HTML source pages,
/// small enough that parsing cannot OOM the worker).
pub const DEFAULT_MAX_BYTES: usize = 1_048_576;
/// Default total request timeout. Matches the `FETCH_TIMEOUT_MS` default.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// The single SSRF-safe fetcher. Built once per call site (client + caps),
/// then `fetch`ed through; per-hop policy lives inside [`SafeFetcher::fetch`]
/// so callers cannot skip it.
#[derive(Debug, Clone)]
pub struct SafeFetcher {
    client: Client,
    max_bytes: usize,
    max_redirects: usize,
    allow_loopback: bool,
}

impl Default for SafeFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SafeFetcher {
    /// Build a fetcher with default caps and timeout, loopback blocked.
    /// The internal client has redirect-following disabled: redirects are
    /// followed manually so each hop is re-checked.
    pub fn new() -> Self {
        Self {
            client: build_fetch_client(DEFAULT_FETCH_TIMEOUT),
            max_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            allow_loopback: false,
        }
    }

    /// Build a fetcher from server config. `allow_loopback` relaxes the SSRF
    /// check for literal loopback addresses only (integration tests hitting
    /// mock servers on 127.0.0.1); production callers pass `false`. Named
    /// hosts (`localhost`, `*.internal`, trailing-dot forms) are still
    /// resolved and checked on every hop either way.
    pub fn from_config(config: &crate::config::Config, allow_loopback: bool) -> Self {
        Self {
            client: build_fetch_client(Duration::from_millis(config.fetch_timeout_ms)),
            max_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            allow_loopback,
        }
    }

    /// Override the total request timeout (tests, avatar fast-path).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = build_fetch_client(timeout);
        self
    }

    /// Override the streaming body cap (tests use a small cap so the
    /// too-large path is exercised without allocating megabytes).
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Override the redirect hop cap.
    pub fn with_max_redirects(mut self, max_redirects: usize) -> Self {
        self.max_redirects = max_redirects;
        self
    }

    /// Relax the SSRF check for literal loopback addresses (test plumbing).
    pub fn with_allow_loopback(mut self, allow_loopback: bool) -> Self {
        self.allow_loopback = allow_loopback;
        self
    }

    /// Fetch `url`, following redirects with a fresh SSRF check per hop.
    pub async fn fetch(&self, url: &Url) -> Result<FetchedDoc, FetchError> {
        let mut current = url.clone();
        for _ in 0..=self.max_redirects {
            self.check_url(&current).await?;
            let mut resp =
                self.client
                    .get(current.clone())
                    .send()
                    .await
                    .map_err(|e| FetchError::Http {
                        url: current.to_string(),
                        source: e,
                    })?;
            let status = resp.status();
            if status.is_redirection() {
                let location = resp
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| FetchError::HttpStatus {
                        url: current.to_string(),
                        status: status.as_u16(),
                    })?
                    .to_str()
                    .map_err(|_| FetchError::HttpStatus {
                        url: current.to_string(),
                        status: status.as_u16(),
                    })?
                    .to_string();
                current = current
                    .join(&location)
                    .map_err(|e| FetchError::InvalidUrl {
                        url: location,
                        source: e,
                    })?;
                continue;
            }
            if status == reqwest::StatusCode::GONE {
                return Err(FetchError::Gone(current.to_string()));
            }
            if !status.is_success() {
                return Err(FetchError::HttpStatus {
                    url: current.to_string(),
                    status: status.as_u16(),
                });
            }
            // Byte cap, enforced while streaming: a Content-Length
            // pre-check avoids reading at all, and the chunk loop aborts
            // the moment the cap is exceeded — the full body is never
            // materialized for oversized responses.
            if let Some(len) = resp
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok())
                && len > self.max_bytes
            {
                return Err(FetchError::TooLarge {
                    url: current.to_string(),
                    limit: self.max_bytes,
                });
            }
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| FetchError::BodyRead {
                url: current.to_string(),
                source: e,
            })? {
                body.extend_from_slice(&chunk);
                if body.len() > self.max_bytes {
                    return Err(FetchError::TooLarge {
                        url: current.to_string(),
                        limit: self.max_bytes,
                    });
                }
            }
            // NOTE: the old path used resp.text() (strict UTF-8, error on
            // invalid bytes); lossy decode keeps backlink matching intact —
            // non-UTF-8 author names may mojibake (accepted).
            let doc = Html::parse_document(&String::from_utf8_lossy(&body));
            return Ok(FetchedDoc {
                url: current,
                status,
                bytes: body,
                doc,
            });
        }
        Err(FetchError::TooManyRedirects(current.to_string()))
    }

    /// Parse `url` then [`SafeFetcher::fetch`] it, so callers keep the same
    /// invalid-URL behavior the old `fetch_url(&str)` had.
    pub async fn fetch_str(&self, url: &str) -> Result<FetchedDoc, FetchError> {
        let parsed = Url::parse(url).map_err(|e| FetchError::InvalidUrl {
            url: url.to_string(),
            source: e,
        })?;
        self.fetch(&parsed).await
    }

    /// Per-hop SSRF gate: host presence, scheme allow-list, then
    /// resolve-and-check unless this is a literal loopback address under
    /// test plumbing.
    async fn check_url(&self, url: &Url) -> Result<(), FetchError> {
        let host = url
            .host_str()
            .ok_or_else(|| FetchError::NoHost(url.to_string()))?;
        // Credentials in the URL are never legitimate for fetching and would
        // otherwise land in error strings and logs: refuse before connecting,
        // without echoing the URL (see the redaction assertion in tests).
        if !url.username().is_empty() || url.password().is_some() {
            return Err(FetchError::Blocked(
                "refused URL with userinfo credentials".to_string(),
            ));
        }
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(FetchError::UnsupportedScheme(url.to_string()));
        }
        if self.allow_loopback && is_loopback_host(host) {
            return Ok(());
        }
        resolve_and_check(host)
            .await
            .map_err(|e| FetchError::Blocked(format!("{url}: {e}")))?;
        Ok(())
    }
}

/// Build the internal client: redirect-following disabled (redirects are
/// followed manually by [`SafeFetcher::fetch`] with per-hop checks).
fn build_fetch_client(timeout: Duration) -> Client {
    Client::builder()
        .user_agent(format!(
            "webmention.nithitsuki.com/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest fetch client build")
}

/// A fetched page: final URL, status, raw bytes, and the single parse tree
/// every consumer (`has_backlink`, `parse_h_entry`, favicon selection) reads
/// from — no double parsing of untrusted HTML.
#[derive(Debug)]
pub struct FetchedDoc {
    /// Final URL after redirects.
    pub url: Url,
    /// HTTP status of the final response (always success when returned).
    pub status: reqwest::StatusCode,
    /// Raw response body, capped at the fetcher's byte limit.
    pub bytes: Vec<u8>,
    /// The body parsed exactly once.
    pub doc: Html,
}

impl FetchedDoc {
    /// Lossy body text (for consumers that still take `&str`, e.g. favicon
    /// selection — which parses again on its own; the hot worker path reads
    /// [`FetchedDoc::doc`] with no re-parse).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// Typed fetch failures. `Blocked` is fail-closed: blocked hosts/IPs and DNS
/// failures all refuse the fetch before any response body is read.
#[derive(Debug)]
pub enum FetchError {
    InvalidUrl {
        url: String,
        source: url::ParseError,
    },
    NoHost(String),
    UnsupportedScheme(String),
    Blocked(String),
    TooManyRedirects(String),
    TooLarge {
        url: String,
        limit: usize,
    },
    Gone(String),
    HttpStatus {
        url: String,
        status: u16,
    },
    Http {
        url: String,
        source: reqwest::Error,
    },
    BodyRead {
        url: String,
        source: reqwest::Error,
    },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl { url, source } => {
                write!(f, "invalid URL {url}: {source}")
            }
            Self::NoHost(url) => write!(f, "no host in URL {url}"),
            Self::UnsupportedScheme(url) => {
                write!(f, "unsupported scheme in URL {url}")
            }
            Self::Blocked(url) => write!(f, "fetch blocked (SSRF): {url}"),
            Self::TooManyRedirects(url) => {
                write!(f, "too many redirects fetching {url}")
            }
            Self::TooLarge { url, limit } => {
                write!(f, "response body from {url} exceeds {limit} bytes")
            }
            Self::Gone(url) => write!(f, "source {url} is gone (410)"),
            Self::HttpStatus { url, status } => {
                write!(f, "HTTP {status} fetching {url}")
            }
            Self::Http { url, source } => write!(f, "HTTP error fetching {url}: {source}"),
            Self::BodyRead { url, source } => {
                write!(f, "failed to read body from {url}: {source}")
            }
        }
    }
}

impl std::error::Error for FetchError {}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TARGET: &str = "https://nithitsuki.com/blog/hello";

    fn source_html() -> String {
        format!(
            r#"<!DOCTYPE html><html><body>
<article class="h-entry"><div class="e-content"><p>Hi!</p></div></article>
<a href="{TARGET}">backlink</a></body></html>"#
        )
    }

    /// Test fetcher: loopback allowed (wiremock lives on 127.0.0.1), small
    /// byte cap so the too-large path is exercised with kilobytes, not
    /// megabytes (no OOM risk in the suite).
    fn test_fetcher() -> SafeFetcher {
        SafeFetcher::new()
            .with_allow_loopback(true)
            .with_max_bytes(8192)
    }

    #[tokio::test]
    async fn redirect_to_localhost_dot_refused() {
        // Named-host redirect bypass: the sync-only policy this replaces
        // follows `http://localhost./` blindly; the per-hop resolve must
        // refuse it even when the first hop was loopback-allowed.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "http://localhost./"),
            )
            .mount(&server)
            .await;
        let err = test_fetcher()
            .fetch_str(&format!("{}/start", server.uri()))
            .await
            .expect_err("redirect to localhost. must be refused");
        assert!(
            matches!(err, FetchError::Blocked(_)),
            "expected Blocked, got: {err}"
        );
    }

    #[tokio::test]
    async fn redirect_loop_fail_closed_within_hop_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
            .mount(&server)
            .await;
        let fetcher = test_fetcher().with_max_redirects(5);
        let err = fetcher
            .fetch_str(&format!("{}/loop", server.uri()))
            .await
            .expect_err("redirect loop must fail closed");
        assert!(
            matches!(err, FetchError::TooManyRedirects(_)),
            "expected TooManyRedirects, got: {err}"
        );
        let received = server.received_requests().await.unwrap_or_default();
        let gets = received.iter().filter(|r| r.method == "GET").count();
        assert!(gets <= 6, "at most max_redirects + 1 requests, got {gets}");
    }

    #[tokio::test]
    async fn body_over_cap_is_too_large_without_oom() {
        // 2× the small test cap: exercises the streaming limit with
        // kilobytes, never megabytes.
        let server = MockServer::start().await;
        let big = "x".repeat(16_384);
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string(&big))
            .mount(&server)
            .await;
        let err = test_fetcher()
            .fetch_str(&format!("{}/big", server.uri()))
            .await
            .expect_err("2×-cap body must be refused");
        match err {
            FetchError::TooLarge { limit, .. } => assert_eq!(limit, 8192),
            other => panic!("expected TooLarge, got: {other}"),
        }
    }

    #[tokio::test]
    async fn chunked_body_without_length_trips_streaming_cap() {
        // Defeats the Content-Length pre-check (chunked framing carries no
        // length), so only the per-chunk loop can refuse the 12 KiB body
        // under the 8 KiB test cap. A minimal raw chunked origin — no new
        // deps, fully deterministic.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
            for _ in 0..3 {
                sock.write_all(b"1000\r\n").await.unwrap();
                sock.write_all(&vec![b'x'; 4096]).await.unwrap();
                sock.write_all(b"\r\n").await.unwrap();
            }
            sock.write_all(b"0\r\n\r\n").await.unwrap();
        });
        let err = test_fetcher()
            .fetch_str(&format!("http://{addr}/chunked"))
            .await
            .expect_err("over-cap chunked body must be refused by the loop");
        match err {
            FetchError::TooLarge { limit, .. } => assert_eq!(limit, 8192),
            other => panic!("expected TooLarge, got: {other}"),
        }
    }

    #[test]
    fn production_default_caps_pinned() {
        // Cheap pins against silent drift of the production defaults.
        assert_eq!(DEFAULT_MAX_BYTES, 1_048_576, "1 MiB body cap");
        assert_eq!(DEFAULT_MAX_REDIRECTS, 5, "five-hop redirect cap");
        assert_eq!(DEFAULT_FETCH_TIMEOUT, Duration::from_secs(4));
        assert_eq!(
            crate::config::Config::default().fetch_timeout_ms,
            4000,
            "fetcher default matches the config default",
        );
    }

    #[tokio::test]
    async fn gone_410_maps_to_gone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;
        let err = test_fetcher()
            .fetch_str(&format!("{}/gone", server.uri()))
            .await
            .expect_err("410 must map to Gone");
        assert!(
            matches!(err, FetchError::Gone(_)),
            "expected Gone, got: {err}"
        );
    }

    #[tokio::test]
    async fn success_returns_single_parsed_doc() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/post"))
            .respond_with(ResponseTemplate::new(200).set_body_string(source_html()))
            .mount(&server)
            .await;
        let fetched = test_fetcher()
            .fetch_str(&format!("{}/post", server.uri()))
            .await
            .expect("small HTML page must fetch");
        assert_eq!(fetched.status, reqwest::StatusCode::OK);
        // Both consumers read the one parse tree — no double parse.
        assert!(crate::mf2::has_backlink(&fetched.doc, TARGET));
        assert!(crate::mf2::parse_h_entry(&fetched.doc).is_some());
    }

    #[tokio::test]
    async fn blocked_before_connect_for_private_targets() {
        // No listener, no DNS trick: the refusal happens in check_url before
        // any socket is opened (fast — would time out otherwise).
        let fetcher = SafeFetcher::new();
        for target in [
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/",
            "http://127.0.0.1:9/",
            "http://[::1]/",
        ] {
            let err = fetcher
                .fetch_str(target)
                .await
                .expect_err(format!("{target} must be refused").as_str());
            assert!(
                matches!(err, FetchError::Blocked(_)),
                "expected Blocked for {target}, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn userinfo_credentials_refused_and_redacted() {
        // Credentials in the URL are never legitimate for fetching: refuse
        // before connecting, and never echo them into the error (worker warn
        // logs print FetchError via Display).
        let fetcher = SafeFetcher::new().with_allow_loopback(true);
        for target in ["http://user:pass@127.0.0.1:9/", "http://user@127.0.0.1:9/"] {
            let err = fetcher
                .fetch_str(target)
                .await
                .expect_err(format!("{target} must be refused").as_str());
            assert!(
                matches!(err, FetchError::Blocked(_)),
                "expected Blocked for userinfo URL, got: {err}"
            );
            let shown = err.to_string();
            assert!(
                !shown.contains("pass") && !shown.contains("user@"),
                "credentials must not leak into Display: {shown}"
            );
        }
    }

    #[tokio::test]
    async fn non_http_scheme_and_missing_host_rejected() {
        let fetcher = SafeFetcher::new();
        let err = fetcher
            .fetch_str("ftp://example.com/file")
            .await
            .expect_err("ftp must be rejected");
        assert!(matches!(err, FetchError::UnsupportedScheme(_)));
        let err = fetcher
            .fetch_str("mailto:someone@example.com")
            .await
            .expect_err("hostless URL must be rejected");
        assert!(matches!(err, FetchError::NoHost(_)));
    }
}
