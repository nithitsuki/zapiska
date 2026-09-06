//! Shared channel seam: one retry policy, one dispatch path, one set of
//! size-budget helpers for every notification channel.
//!
//! Each channel (Telegram / Slack / Discord, see the sibling modules) is a
//! thin [`Channel`] adapter that owns only its wire format and its escape
//! rules. Everything else — when to send, how many attempts, with what
//! budget — lives here. Adding a fourth channel is one new adapter file plus
//! the registration checklist in `Notifier::channels` (`Config` fields,
//! `Notifier` fields plus an `is_empty` arm, one `push` line).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::{Client, StatusCode};

/// Total delivery attempts per message (initial try + retries).
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Base backoff between attempts; attempt N sleeps `N * base` (100 ms, 200 ms).
pub(crate) const RETRY_BASE_DELAY: Duration = Duration::from_millis(100);
/// Cap on honored `Retry-After` waits: a punitive server must not stall a
/// delivery (or the shutdown drain) beyond a few seconds per wait.
pub(crate) const RETRY_AFTER_MAX: Duration = Duration::from_secs(2);
/// Per-attempt request timeout (the old per-channel 10 s, now in one place).
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// One outbound webhook POST: every channel delivers as JSON.
#[derive(Debug, Clone)]
pub(crate) struct Outgoing {
    pub url: String,
    pub body: serde_json::Value,
}

/// One notification channel adapter.
///
/// Adapters own the wire format (`format_*`) and the escape policy (each
/// sibling module keeps its own `escape`, unchanged). Retry, timeout, budget
/// helpers, and logging are shared here and in `mod.rs`'s dispatch loop.
pub(crate) trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    /// Hard channel limit in characters (Telegram 4096, Slack 3000,
    /// Discord 2000). Formatters shrink to fit; see the budget helpers.
    /// Read by seam tests (and future adapters); the per-channel builders
    /// enforce it via their own `*_MAX_CHARS` constant.
    #[allow(dead_code)]
    fn max_chars(&self) -> usize;
    fn format_single(&self, info: &super::NewCommentInfo) -> Outgoing;
    fn format_digest(&self, digest: &super::Digest) -> Outgoing;
    /// Whether a 2xx body must be validated (Telegram's `ok: true`).
    fn needs_body_check(&self) -> bool {
        false
    }
    /// Validate a 2xx body; only called when `needs_body_check` is true.
    fn check_body(&self, _body: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
}

/// Retryable = transient: rate limits and server errors. Other 4xx and
/// API-logic errors (`ok: false`) are permanent — one attempt, then drop.
pub(crate) fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Wait before the next attempt: the attempt backoff, raised to the server's
/// `Retry-After` (delta-seconds only; capped at [`RETRY_AFTER_MAX`]). Pure —
/// unit-tested directly.
pub(crate) fn retry_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    let base = RETRY_BASE_DELAY * attempt;
    match retry_after_secs {
        Some(secs) => base.max(Duration::from_secs(secs).min(RETRY_AFTER_MAX)),
        None => base,
    }
}

/// Parse a `Retry-After` delta-seconds value. HTTP-dates and garbage yield
/// `None` (the attempt backoff applies instead).
fn retry_after_secs(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Tracks spawned (fire-and-forget) sends so the shutdown drain can await
/// them. Counter version: the increment happens synchronously before
/// `tokio::spawn`, so a drain that observes zero either started after the
/// send finished (nothing to wait for) or before it started (the increment
/// lands first and the wait sees it). Shared by dispatch (which spawns) and
/// drain (which waits) via `Notifier`.
#[derive(Debug, Default)]
pub(crate) struct Inflight {
    count: AtomicU64,
    idle: tokio::sync::Notify,
}

impl Inflight {
    fn started(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }

    fn finished(&self) {
        if self.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }

    /// Resolve when no spawned send is outstanding. The waiter registers
    /// before checking the count, so a concurrent finish cannot slip
    /// between the check and the sleep.
    pub(crate) async fn idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// POST once per attempt, up to [`MAX_ATTEMPTS`], backing off between
/// transient failures. Bounded and non-blocking-friendly: the caller decides
/// whether to await (shutdown drain) or spawn (hot path). Bounded retry
/// (duplicates possible on timeout), log-and-drop: a persistent failure
/// returns `Err` for the caller to log and drop.
pub(crate) async fn post_with_retry(
    client: &Client,
    channel: &dyn Channel,
    msg: &Outgoing,
) -> Result<StatusCode, String> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match client
            .post(&msg.url)
            .json(&msg.body)
            .timeout(SEND_TIMEOUT)
            .send()
            .await
        {
            Err(e) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(format!("request failed after {attempt} attempts: {e}"));
                }
                tokio::time::sleep(RETRY_BASE_DELAY * attempt).await;
            }
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    if is_retryable(status) && attempt < MAX_ATTEMPTS {
                        let wait = retry_delay(attempt, retry_after_secs(&resp));
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(format!("HTTP {status} after {attempt} attempt(s)"));
                }
                if channel.needs_body_check() {
                    let body: serde_json::Value = resp
                        .json()
                        .await
                        .map_err(|e| format!("invalid response: {e}"))?;
                    channel.check_body(&body)?;
                }
                return Ok(status);
            }
        }
    }
}

/// Fire-and-forget delivery with the shared retry policy. Failures are logged
/// with the channel name and dropped — never surfaced to the comment path.
/// Tracked in `inflight` so shutdown drain can await outstanding sends.
pub(crate) fn spawn_delivery(
    client: &Client,
    inflight: &Arc<Inflight>,
    channel: Box<dyn Channel>,
    msg: Outgoing,
) {
    let client = client.clone();
    let inflight = Arc::clone(inflight);
    inflight.started();
    tokio::spawn(async move {
        log_result(
            channel.name(),
            &post_with_retry(&client, &*channel, &msg).await,
        );
        inflight.finished();
    });
}

/// Shared delivery log line, so every channel reports success and bounded
/// drops identically.
pub(crate) fn log_result(channel: &str, result: &Result<StatusCode, String>) {
    match result {
        Ok(status) => tracing::debug!(channel, status = %status, "notification sent"),
        Err(e) => {
            tracing::warn!(channel, err = %e, "notification failed, dropping after retry budget")
        }
    }
}

// ── Shared size-budget helpers ───────────────────────────────
// One shrinking policy for all channels: formatters shrink the cheapest
// fields first (preview text, then names) and keep the moderation footer;
// only the last-resort `hard_truncate` can cut it.

/// Shorten a non-empty field by at least `overflow` characters, marking the
/// cut with `...`. Always returns a strictly shorter string, so budget loops
/// terminate.
pub(crate) fn shrink_text(field: &str, overflow: usize) -> String {
    let len = field.chars().count();
    let keep = len.saturating_sub(overflow + 3);
    if keep == 0 {
        return String::new();
    }
    let mut out: String = field.chars().take(keep).collect();
    while out.ends_with('\\') {
        out.pop();
    }
    if out.trim().is_empty() {
        return String::new();
    }
    out.push_str("...");
    out
}

/// Shorten an unescaped raw field the same way, without the backslash guard
/// (Telegram/Slack escaping happens after shrinking).
pub(crate) fn shrink_raw(field: &str, overflow: usize) -> String {
    let len = field.chars().count();
    let keep = len.saturating_sub(overflow + 3);
    if keep == 0 {
        return String::new();
    }
    let out: String = field.chars().take(keep).collect();
    if out.trim().is_empty() {
        return String::new();
    }
    out + "..."
}

/// Last-resort cap: cut the whole message to `max` characters. Only reached
/// when every shrinkable field is already empty.
pub(crate) fn hard_truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max - 3).collect::<String>() + "..."
}

/// Index of the longest non-empty string in `fields`, if any.
pub(crate) fn longest_field<'a>(fields: impl Iterator<Item = &'a String>) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, f) in fields.enumerate() {
        let len = f.chars().count();
        if len == 0 {
            continue;
        }
        if best.is_none_or(|(_, best_len)| len > best_len) {
            best = Some((i, len));
        }
    }
    best.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use reqwest::Client;

    use super::{
        Channel, MAX_ATTEMPTS, Outgoing, hard_truncate, longest_field, post_with_retry, shrink_text,
    };
    use crate::notify::{Digest, DigestPreview, NewCommentInfo};

    /// Minimal test adapter: Discord-shaped (no body check), delivered to the
    /// given URL. Keeps retry tests independent of any real channel.
    struct StubChannel {
        url: String,
    }
    impl Channel for StubChannel {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn max_chars(&self) -> usize {
            2000
        }
        fn format_single(&self, _info: &NewCommentInfo) -> Outgoing {
            Outgoing {
                url: self.url.clone(),
                body: serde_json::json!({"content": "hi"}),
            }
        }
        fn format_digest(&self, _digest: &Digest) -> Outgoing {
            Outgoing {
                url: self.url.clone(),
                body: serde_json::json!({"content": "digest"}),
            }
        }
    }

    fn stub_outgoing(url: &str) -> Outgoing {
        Outgoing {
            url: url.to_string(),
            body: serde_json::json!({"content": "hi"}),
        }
    }

    /// Axum stub whose first `fail_first` POSTs answer 500 and later ones 200.
    /// Deterministic where wiremock mock-ordering would not be.
    async fn counting_server(fail_first: usize) -> (String, Arc<AtomicUsize>) {
        counting_server_with(
            fail_first,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            None,
        )
        .await
    }

    /// Variant answering `status` (with an optional `Retry-After` header)
    /// for the first `fail_first` POSTs, then 200.
    async fn counting_server_with(
        fail_first: usize,
        status: axum::http::StatusCode,
        retry_after_secs: Option<u64>,
    ) -> (String, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                    if n <= fail_first {
                        let mut headers = axum::http::HeaderMap::new();
                        if let Some(secs) = retry_after_secs {
                            headers.insert(
                                axum::http::header::RETRY_AFTER,
                                secs.to_string().parse().unwrap(),
                            );
                        }
                        (status, headers, "slow down".to_string())
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            axum::http::HeaderMap::new(),
                            "ok".to_string(),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/"), count)
    }

    /// Body-checking adapter (Telegram-shaped): 2xx bodies must carry
    /// `ok: true`, mirroring `TelegramChannel::check_body` without reaching
    /// into the private telegram module.
    struct OkFalseChannel {
        url: String,
    }
    impl Channel for OkFalseChannel {
        fn name(&self) -> &'static str {
            "okfalse"
        }
        fn max_chars(&self) -> usize {
            2000
        }
        fn format_single(&self, _info: &NewCommentInfo) -> Outgoing {
            stub_outgoing(&self.url)
        }
        fn format_digest(&self, _digest: &Digest) -> Outgoing {
            stub_outgoing(&self.url)
        }
        fn needs_body_check(&self) -> bool {
            true
        }
        fn check_body(&self, body: &serde_json::Value) -> Result<(), String> {
            if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                return Err(format!("API error: {body}"));
            }
            Ok(())
        }
    }

    /// Axum stub answering `{"ok": false}` once, then `{"ok": true}`.
    async fn ok_false_once_server() -> (String, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let seen = Arc::clone(&seen);
                async move {
                    let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                    axum::Json(serde_json::json!({"ok": n > 1}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/"), count)
    }

    #[tokio::test]
    async fn retry_delivers_after_two_transient_failures() {
        let (url, count) = counting_server(2).await;
        let client = Client::new();
        let channel = StubChannel { url: url.clone() };
        let out = stub_outgoing(&url);
        post_with_retry(&client, &channel, &out)
            .await
            .expect("third attempt must succeed");
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "two 500s then a success = three attempts"
        );
    }

    #[tokio::test]
    async fn persistent_5xx_drops_after_max_attempts_without_panic() {
        let (url, count) = counting_server(usize::MAX).await;
        let client = Client::new();
        let channel = StubChannel { url: url.clone() };
        let out = stub_outgoing(&url);
        let err = post_with_retry(&client, &channel, &out)
            .await
            .expect_err("always-500 must surface an error for the logged drop");
        assert!(
            err.contains("500"),
            "drop reason must name the status, got: {err}"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "bounded: exactly N attempts, then a logged drop"
        );
    }

    #[tokio::test]
    async fn client_error_is_not_retried() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(400))
            .mount(&server)
            .await;
        let client = Client::new();
        let channel = StubChannel { url: server.uri() };
        let out = stub_outgoing(&server.uri());
        post_with_retry(&client, &channel, &out)
            .await
            .expect_err("400 is permanent, not retryable");
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(reqs.len(), 1, "a 400 must be attempted exactly once");
    }

    #[test]
    fn budget_helpers_shrink_and_cap() {
        let shrunk = shrink_text("abcdefghij", 4);
        assert!(
            shrunk.chars().count() < 10 && shrunk.ends_with("..."),
            "shrink must shorten and mark, got: {shrunk}"
        );
        let capped = hard_truncate(&"x".repeat(5000), 2000);
        assert_eq!(capped.chars().count(), 2000);
        assert!(capped.ends_with("..."));
        let fields = ["a".to_string(), "abcd".to_string(), "ab".to_string()];
        assert_eq!(longest_field(fields.iter()), Some(1));
        let empty: Vec<String> = vec![];
        assert_eq!(longest_field(empty.iter()), None);
    }

    #[test]
    fn stub_adapter_covers_format_contract() {
        let channel = StubChannel {
            url: "http://example.invalid/".to_string(),
        };
        let info = NewCommentInfo {
            id: 1,
            target_path: "/p".to_string(),
            comment_type: "native".to_string(),
            author_name: "A".to_string(),
            author_url: None,
            content: "hi".to_string(),
            honeypot: false,
            is_reply: false,
        };
        assert_eq!(channel.format_single(&info).url, channel.url);
        let digest = Digest {
            key: "/p".to_string(),
            count: 1,
            commenters: vec!["A".to_string()],
            previews: vec![DigestPreview {
                author: "A".to_string(),
                target_path: "/p".to_string(),
                content: "hi".to_string(),
            }],
        };
        // The stub proves the trait contract compiles for both message shapes.
        assert_eq!(channel.format_digest(&digest).url, channel.url);
    }

    #[test]
    fn retry_bound_is_sane() {
        assert!(
            (2..=5).contains(&MAX_ATTEMPTS),
            "retry bound must stay small and bounded, got {MAX_ATTEMPTS}"
        );
        assert!(
            Duration::from_millis(50) <= super::RETRY_BASE_DELAY
                && super::RETRY_BASE_DELAY <= Duration::from_secs(1),
            "backoff base must stay in the snappy range"
        );
    }

    #[test]
    fn retry_delay_defaults_to_backoff_without_header() {
        assert_eq!(super::retry_delay(1, None), Duration::from_millis(100));
        assert_eq!(super::retry_delay(2, None), Duration::from_millis(200));
    }

    #[test]
    fn retry_delay_honors_retry_after_within_cap() {
        assert_eq!(super::retry_delay(1, Some(1)), Duration::from_secs(1));
    }

    #[test]
    fn retry_delay_caps_huge_retry_after() {
        assert_eq!(
            super::retry_delay(1, Some(120)),
            super::RETRY_AFTER_MAX,
            "a punitive Retry-After must not stall past the cap"
        );
    }

    #[test]
    fn retry_delay_never_below_backoff() {
        assert_eq!(super::retry_delay(2, Some(0)), Duration::from_millis(200));
    }

    #[tokio::test]
    async fn retry_after_header_is_honored() {
        let (url, count) =
            counting_server_with(1, axum::http::StatusCode::TOO_MANY_REQUESTS, Some(1)).await;
        let client = Client::new();
        let channel = StubChannel { url: url.clone() };
        let out = stub_outgoing(&url);
        let start = std::time::Instant::now();
        post_with_retry(&client, &channel, &out)
            .await
            .expect("second attempt must succeed");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(
            start.elapsed() >= Duration::from_millis(900),
            "Retry-After: 1 must be honored, not just the 100 ms base (took {:?})",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn ok_false_once_is_permanent() {
        // The first answer is ok:false, later ones ok:true — delivery still
        // drops after ONE attempt: a logic refusal is permanent, even when
        // success was one attempt away.
        let (url, count) = ok_false_once_server().await;
        let client = Client::new();
        let channel = OkFalseChannel { url: url.clone() };
        let out = stub_outgoing(&url);
        let err = post_with_retry(&client, &channel, &out)
            .await
            .expect_err("ok:false must surface for the logged drop");
        assert!(err.contains("API error"), "got: {err}");
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "a logic refusal must be attempted exactly once"
        );
    }
}
