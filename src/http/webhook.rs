//! Fire-and-forget webhook delivery, shared by comment and reaction events,
//! plus HMAC payload signing (S3 trust edge).
//!
//! Signing scheme (additive: empty secret = unsigned, byte-identical to the
//! old behavior):
//! - Message: `{timestamp_secs}.{raw JSON body bytes}`, HMAC-SHA256 keyed by
//!   `WEBHOOK_SIGNING_SECRET`, hex-encoded as `v1=<hex>`.
//! - Headers: `X-Zapiska-Timestamp: <unix secs>` and
//!   `X-Zapiska-Signature: v1=<hex>` on BOTH async (`fire`) and sync
//!   (`request_decision`) emissions, so one shared implementation covers both
//!   modes (T19 dedup point with `comment_post`/`reactions`).
//! - Verification: the consumer recomputes over the received bytes, compares
//!   in constant time, and rejects timestamps outside ±[`REPLAY_WINDOW_SECS`].
//!   A consumer holding a secret rejects unsigned or tampered bodies; a
//!   consumer with no secret accepts everything (backwards compatible).
//! - No new dependencies: HMAC-SHA256 is built on the existing `sha2` crate
//!   (ipad/opad construction), comparison on the existing `subtle` crate.

/// Replay window for signed webhooks: ±5 minutes around the consumer's clock.
/// Consumer-side contract (verified by the external service, not by this
/// crate's hot path — hence `dead_code`-allowed outside tests).
#[allow(dead_code)]
pub const REPLAY_WINDOW_SECS: u64 = 300;

/// Timestamp header carrying the signing instant (unix seconds, ASCII).
pub const TIMESTAMP_HEADER: &str = "x-zapiska-timestamp";
/// Signature header carrying `v1=<hex hmac>`.
pub const SIGNATURE_HEADER: &str = "x-zapiska-signature";

/// HMAC-SHA256 over (`key`, `msg`) via the standard ipad/opad construction.
/// Long keys hash down first, per RFC 2104.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(key);
        let digest = h.finalize();
        k[..32].copy_from_slice(&digest);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    Some(out)
}

/// Current unix time in whole seconds (signing instant / verification clock).
#[must_use]
pub fn timestamp_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Sign `body` at `timestamp_secs` under `secret`. Returns the header value
/// (`v1=<hex>`). Pure: the same inputs always produce the same output, so
/// consumers and tests can recompute independently.
#[must_use]
pub fn sign_body(secret: &str, timestamp_secs: u64, body: &[u8]) -> String {
    let mut msg = timestamp_secs.to_string().into_bytes();
    msg.push(b'.');
    msg.extend_from_slice(body);
    format!("v1={}", hex_encode(&hmac_sha256(secret.as_bytes(), &msg)))
}

/// Verify a received body against its headers at `now_secs`. `timestamp_hdr`
/// is the raw `X-Zapiska-Timestamp` value, `sig_hdr` the raw
/// `X-Zapiska-Signature` value. Rejects missing/malformed headers, clock
/// skew beyond [`REPLAY_WINDOW_SECS`], and constant-time-unequal MACs.
/// Consumer-side: the external moderation service calls this (or recomputes
/// per the documented scheme); this crate's hot path only signs.
#[allow(dead_code)]
#[must_use]
pub fn verify_body(
    secret: &str,
    timestamp_hdr: Option<&str>,
    sig_hdr: Option<&str>,
    body: &[u8],
    now_secs: u64,
) -> bool {
    if secret.is_empty() {
        return true; // unsigned mode: nothing to check
    }
    let (Some(ts_raw), Some(sig_raw)) = (timestamp_hdr, sig_hdr) else {
        return false; // secret set but the emission is unsigned
    };
    let Ok(ts) = ts_raw.trim().parse::<u64>() else {
        return false;
    };
    if ts.abs_diff(now_secs) > REPLAY_WINDOW_SECS {
        return false; // replay outside the window
    }
    let Some(hex) = sig_raw.trim().strip_prefix("v1=") else {
        return false;
    };
    let (Some(got), expected) = (hex_decode(hex), sign_body(secret, ts, body)) else {
        return false;
    };
    let Some(expected_hex) = expected.strip_prefix("v1=").and_then(hex_decode) else {
        return false;
    };
    use subtle::ConstantTimeEq;
    got.ct_eq(&expected_hex).into()
}

/// POST a JSON payload to `url` on a spawned task. Failures are logged at
/// `warn` and never affect the caller. When `signing_secret` is `Some`
/// (non-empty), the spawn carries the timestamp/signature headers (async
/// half of the "sign BOTH emissions" contract); `None`/empty sends the
/// historical unsigned body.
pub(crate) fn fire(
    client: &reqwest::Client,
    url: &str,
    payload: serde_json::Value,
    timeout_secs: u64,
    signing_secret: Option<&str>,
) {
    let client = client.clone();
    let url = url.to_string();
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let signed = match signing_secret {
        Some(s) if !s.is_empty() => {
            let ts = timestamp_now();
            Some((ts.to_string(), sign_body(s, ts, &body)))
        }
        _ => None,
    };
    tokio::spawn(async move {
        let mut req = client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .timeout(std::time::Duration::from_secs(timeout_secs));
        if let Some((ts, sig)) = signed {
            req = req
                .header(TIMESTAMP_HEADER, ts)
                .header(SIGNATURE_HEADER, sig);
        }
        match req.send().await {
            Ok(r) => {
                tracing::debug!(webhook = %url, status = %r.status(), "webhook sent")
            }
            Err(e) => tracing::warn!(webhook = %url, err = %e, "webhook failed"),
        }
    });
}

/// Sync POST with a 10 s timeout, returning the raw response body on success.
/// Signs exactly like [`fire`] (sync half of the contract). `None` on any
/// transport failure or non-2xx status. The signature covers the
/// already-serialized transmit buffer — never a second serialization of the
/// `Value` (structural invariant, not coincidental determinism).
pub(crate) async fn post_signed(
    client: &reqwest::Client,
    url: &str,
    payload: &serde_json::Value,
    signing_secret: Option<&str>,
) -> Option<serde_json::Value> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let mut req = client.post(url).header("content-type", "application/json");
    if let Some(s) = signing_secret {
        if !s.is_empty() {
            let ts = timestamp_now();
            let sig = sign_body(s, ts, &body);
            req = req
                .header(TIMESTAMP_HEADER, ts.to_string())
                .header(SIGNATURE_HEADER, sig);
        }
    }
    req = req.body(body).timeout(std::time::Duration::from_secs(10));
    match req.send().await {
        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.ok(),
        Ok(r) => {
            tracing::warn!(webhook = %url, status = %r.status(), "sync webhook returned error");
            None
        }
        Err(e) => {
            tracing::warn!(webhook = %url, err = %e, "sync webhook failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_round_trip() {
        // RED (S3): no signing existed before T19.
        let body = br#"{"event":"comment.created","id":42}"#;
        let ts = 1_786_000_000u64;
        let sig = sign_body("s3cr3t", ts, body);
        assert!(sig.starts_with("v1="));
        assert!(verify_body(
            "s3cr3t",
            Some(&ts.to_string()),
            Some(&sig),
            body,
            ts
        ));
    }

    #[test]
    fn tampered_body_rejected() {
        let body = br#"{"event":"comment.created","id":42}"#;
        let ts = 1_786_000_000u64;
        let sig = sign_body("s3cr3t", ts, body);
        assert!(!verify_body(
            "s3cr3t",
            Some(&ts.to_string()),
            Some(&sig),
            br#"{"event":"comment.created","id":43}"#,
            ts
        ));
    }

    #[test]
    fn unsigned_when_secret_set_rejected() {
        let body = br#"{"event":"x"}"#;
        assert!(!verify_body("s3cr3t", None, None, body, timestamp_now()));
        assert!(!verify_body(
            "s3cr3t",
            Some("1786000000"),
            None,
            body,
            1_786_000_000
        ));
    }

    #[test]
    fn unsigned_mode_accepts_everything() {
        assert!(verify_body("", None, None, b"anything", 0));
    }

    #[test]
    fn replay_outside_window_rejected() {
        let body = br#"{"event":"x"}"#;
        let ts = 1_786_000_000u64;
        let sig = sign_body("s3cr3t", ts, body);
        assert!(!verify_body(
            "s3cr3t",
            Some(&ts.to_string()),
            Some(&sig),
            body,
            ts + REPLAY_WINDOW_SECS + 1
        ));
        assert!(verify_body(
            "s3cr3t",
            Some(&ts.to_string()),
            Some(&sig),
            body,
            ts + REPLAY_WINDOW_SECS
        ));
    }

    #[test]
    fn wrong_secret_rejected() {
        let body = br#"{"event":"x"}"#;
        let ts = 1_786_000_000u64;
        let sig = sign_body("s3cr3t", ts, body);
        assert!(!verify_body(
            "other",
            Some(&ts.to_string()),
            Some(&sig),
            body,
            ts
        ));
    }

    #[test]
    fn malformed_headers_rejected() {
        let body = br#"{"event":"x"}"#;
        assert!(!verify_body(
            "s",
            Some("not-a-time"),
            Some("v1=abcd"),
            body,
            0
        ));
        assert!(!verify_body("s", Some("1"), Some("bare-hex"), body, 1));
        assert!(!verify_body("s", Some("1"), Some("v1=xyz"), body, 1));
    }

    #[tokio::test]
    async fn async_fire_carries_signature_headers() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        fire(
            &client,
            &server.uri(),
            serde_json::json!({"event": "comment.created"}),
            5,
            Some("s3cr3t"),
        );
        let mut reqs = Vec::new();
        for _ in 0..100 {
            reqs = server.received_requests().await.unwrap_or_default();
            if !reqs.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(reqs.len(), 1);
        let ts = reqs[0]
            .headers
            .get(TIMESTAMP_HEADER)
            .expect("timestamp header");
        let sig = reqs[0]
            .headers
            .get(SIGNATURE_HEADER)
            .expect("signature header");
        assert!(verify_body(
            "s3cr3t",
            Some(ts.to_str().unwrap()),
            Some(sig.to_str().unwrap()),
            &reqs[0].body,
            timestamp_now()
        ));
    }

    #[tokio::test]
    async fn async_fire_unsigned_sends_no_headers() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        fire(
            &client,
            &server.uri(),
            serde_json::json!({"event": "x"}),
            5,
            None,
        );
        let mut reqs = Vec::new();
        for _ in 0..100 {
            reqs = server.received_requests().await.unwrap_or_default();
            if !reqs.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].headers.get(TIMESTAMP_HEADER).is_none());
        assert!(reqs[0].headers.get(SIGNATURE_HEADER).is_none());
    }

    #[tokio::test]
    async fn sync_post_carries_signature_headers() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"action": "approved"})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let out = post_signed(
            &client,
            &server.uri(),
            &serde_json::json!({"event": "comment.created"}),
            Some("s3cr3t"),
        )
        .await;
        assert_eq!(out.unwrap()["action"], "approved");
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        // The signature MUST verify against the TRANSMITTED bytes (not a
        // re-serialization): this is the structural pin for the sign-the-
        // buffer invariant.
        let ts = reqs[0].headers.get(TIMESTAMP_HEADER).expect("ts header");
        let sig = reqs[0].headers.get(SIGNATURE_HEADER).expect("sig header");
        assert!(verify_body(
            "s3cr3t",
            Some(ts.to_str().unwrap()),
            Some(sig.to_str().unwrap()),
            &reqs[0].body,
            timestamp_now()
        ));
    }
}
