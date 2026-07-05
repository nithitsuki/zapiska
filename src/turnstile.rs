//! Cloudflare Turnstile server-side verification.
//!
//! Verifies a Turnstile token returned by the browser widget by calling the
//! Cloudflare `siteverify` endpoint from zapiska's backend. The secret is held
//! in memory (loaded from `TURNSTILE_SECRET_KEY` at startup) and never logged
//! or persisted to disk.
//!
//! Verification is only performed when `TURNSTILE_ENABLED=true`. When disabled,
//! `create_comment` ignores the `cf-turnstile-response` form field entirely.

use std::net::IpAddr;

use reqwest::Client;
use serde::Deserialize;

const DEFAULT_SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// Result of a siteverify call. Cloudflare returns 200 even on verification
/// failure — we surface success/failure via the `success` boolean.
#[derive(Debug)]
pub struct VerificationResult {
    pub success: bool,
    pub error_codes: Vec<String>,
}

/// Verify a Turnstile token against the configured siteverify endpoint.
///
/// - `client` — the shared reqwest client used elsewhere in zapiska. For the
///   non-webmention build it's a plain client (still TLS-only via rustls).
/// - `verify_url` — typically `https://challenges.cloudflare.com/turnstile/v0/siteverify`.
///   Override via `TURNSTILE_VERIFY_URL` for tests / proxies.
/// - `secret` — the Turnstile *secret* key (never the public sitekey).
/// - `token` — the value of the form's `cf-turnstile-response` field.
/// - `remoteip` — optional submitter IP, forwarded to Cloudflare for abuse signals.
///
/// Returns `Err` only on transport failure (timeout, DNS, non-2xx). A
/// verification *failure* (e.g. expired or replayed token) is `Ok(VerificationResult
/// { success: false, .. })` — caller decides how to treat it.
pub async fn verify(
    client: &Client,
    verify_url: &str,
    secret: &str,
    token: &str,
    remoteip: Option<&IpAddr>,
) -> Result<VerificationResult, reqwest::Error> {
    let mut form = vec![
        ("secret", secret.to_string()),
        ("response", token.to_string()),
    ];
    if let Some(ip) = remoteip {
        form.push(("remoteip", ip.to_string()));
    }

    let resp = client
        .post(verify_url)
        .form(&form)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json::<SiteverifyResponse>()
        .await?;

    Ok(VerificationResult {
        success: resp.success,
        error_codes: resp
            .error_codes
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect(),
    })
}

pub fn default_verify_url() -> &'static str {
    DEFAULT_SITEVERIFY_URL
}

#[derive(Debug, Deserialize)]
struct SiteverifyResponse {
    success: bool,
    #[serde(rename = "error-codes", default)]
    error_codes: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client() -> Client {
        Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn verify_success_returns_ok_true() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "error-codes": []
            })))
            .mount(&server)
            .await;

        let res = verify(
            &client(),
            &format!("{}/verify", server.uri()),
            "secret",
            "good-token",
            None,
        )
        .await
        .expect("request should succeed");

        assert!(res.success);
        assert!(res.error_codes.is_empty());
    }

    #[tokio::test]
    async fn verify_failure_returns_ok_false_with_error_codes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": false,
                "error-codes": ["invalid-input-response", "timeout-or-duplicate"]
            })))
            .mount(&server)
            .await;

        let res = verify(
            &client(),
            &format!("{}/verify", server.uri()),
            "secret",
            "stale-token",
            None,
        )
        .await
        .expect("request should succeed");

        assert!(!res.success);
        assert_eq!(
            res.error_codes,
            vec!["invalid-input-response", "timeout-or-duplicate"]
        );
    }

    #[tokio::test]
    async fn verify_sends_secret_response_and_remoteip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true
            })))
            .mount(&server)
            .await;

        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let _ = verify(
            &client(),
            &format!("{}/verify", server.uri()),
            "my-secret",
            "tok",
            Some(&ip),
        )
        .await
        .unwrap();

        let received = &server.received_requests().await.unwrap()[0];
        let body = std::str::from_utf8(&received.body).unwrap();
        assert!(body.contains("secret=my-secret"), "body: {body}");
        assert!(body.contains("response=tok"), "body: {body}");
        assert!(body.contains("remoteip=203.0.113.7"), "body: {body}");
    }

    #[tokio::test]
    async fn verify_returns_err_on_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/verify"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let res = verify(
            &client(),
            &format!("{}/verify", server.uri()),
            "secret",
            "tok",
            None,
        )
        .await;

        assert!(res.is_err(), "expected transport error on 500");
    }
}
