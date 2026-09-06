//! Windowed batching of new-comment notifications.
//!
//! Instead of one message per comment, comments are collected per page (or
//! site-wide, see `NotificationBatcher::global`) into fixed time windows. A
//! single aggregated digest is delivered when the window closes (at
//! `opened_at + window`, regardless of later traffic), or earlier when the
//! mid-window threshold is reached. One timer task is armed per window, when
//! the window opens — never one per push. In-memory; batches still open at
//! shutdown are flushed by [`NotificationBatcher::drain`], which the
//! graceful-shutdown path in `main.rs` awaits.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::Client;

use super::{
    Digest, GLOBAL_KEY, MAX_STORED_PREVIEWS, NewCommentInfo, Notifier, deliver_digest_sync,
    deliver_digest_to_channels, deliver_new_comment,
};
use crate::config::{BatchGranularity, Config};

/// Overall shutdown-drain bound. Worst case without it is one wedged channel
/// at ~3 attempts × 10 s per-attempt timeout (≈30 s, per channel if
/// sequential); 12 s keeps shutdown far under systemd's 90 s default while
/// giving healthy channels room. Stragglers are abandoned (their detached
/// tasks log-and-drop on their own timeouts).
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Clone)]
pub(crate) struct BatchEntry {
    pub(crate) opened_at: Instant,
    /// Comment previews kept for the digest (capped at `MAX_STORED_PREVIEWS`).
    pub(crate) infos: Vec<NewCommentInfo>,
    /// Total comments in this window (may exceed `infos.len()`).
    pub(crate) count: u64,
}

/// Collects new comments into time windows and flushes one digest per window.
/// Shared via `Arc`: clones observe the same batches.
pub struct NotificationBatcher {
    notifier: Notifier,
    /// Window length. `0` = immediate delivery, no batching.
    window: Duration,
    /// Mid-window flush threshold. `0` = window-based only.
    threshold: u64,
    /// `true` = single site-wide window; `false` = one window per page.
    global: bool,
    /// Time source: system clock in production, fake clock in tests.
    clock: Arc<dyn Clock>,
    /// Timer tasks armed (one per window). Observed by tests; proves the
    /// flood case arms O(windows) timers, not O(comments).
    timer_spawns: AtomicU64,
    state: Mutex<HashMap<String, BatchEntry>>,
}

/// Time source for window bookkeeping. Constructor-injected so tests can wind
/// the batcher by hand: comments arrive, the fake clock turns, due windows
/// flush via [`NotificationBatcher::flush_due`].
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Instant;
}

/// Production clock: the system monotonic clock.
#[derive(Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock: an instant wound by hand. `#[cfg(test)]` — production always
/// uses [`SystemClock`].
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FakeClock {
    now: Mutex<Instant>,
}

#[cfg(test)]
impl FakeClock {
    pub(crate) fn new(start: Instant) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    pub(crate) fn advance(&self, delta: Duration) {
        *self.now.lock().expect("fake clock lock") += delta;
    }

    pub(crate) fn now(&self) -> Instant {
        *self.now.lock().expect("fake clock lock")
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.now.lock().expect("fake clock lock")
    }
}

impl NotificationBatcher {
    pub fn new(config: &Config) -> Self {
        Self::with_clock(config, Arc::new(SystemClock))
    }

    /// Test-only window override, applied before wrapping in `Arc` (the
    /// production window comes from `NOTIFY_BATCH_SECS`, whole seconds —
    /// too coarse for the real-timer test).
    #[cfg(test)]
    pub(crate) fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// Production constructor with an injected [`Clock`]. Tests pass a
    /// [`FakeClock`]; everything else behaves identically.
    pub fn with_clock(config: &Config, clock: Arc<dyn Clock>) -> Self {
        Self {
            notifier: Notifier::new(config),
            window: Duration::from_secs(config.notify_batch_secs),
            threshold: config.notify_batch_threshold as u64,
            global: matches!(config.notify_batch_granularity, BatchGranularity::Global),
            clock,
            timer_spawns: AtomicU64::new(0),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Any channel is configured (regardless of batching mode).
    pub fn has_channels(&self) -> bool {
        !self.notifier.is_empty()
    }

    /// Timer tasks armed so far (one per window opened). Test-only read;
    /// production only increments (the counter itself is the flood-case
    /// proof, asserted by `one_timer_per_window_not_per_push`).
    #[cfg(test)]
    pub(crate) fn timer_spawns(&self) -> u64 {
        self.timer_spawns.load(Ordering::Relaxed)
    }

    /// Record a new comment and (eventually) notify the configured channels.
    /// Takes `&Arc<Self>` so window-flush tasks can hold a strong reference.
    pub fn push(self: &Arc<Self>, client: &Client, info: NewCommentInfo) {
        if !self.has_channels() {
            return;
        }

        // Immediate mode: deliver this comment on its own.
        if self.window.is_zero() {
            deliver_new_comment(client, &self.notifier, &info);
            return;
        }

        let key = if self.global {
            GLOBAL_KEY.to_string()
        } else {
            info.target_path.clone()
        };

        enum Action {
            /// A new window opened: arm its single timer.
            ArmTimer,
            /// Appended to an open window: its timer is already armed.
            NoTimer,
            FlushNow(BatchEntry),
        }

        let action = {
            // Scoped guard: never held across an await.
            let mut state = self.state.lock().expect("notify batcher lock");
            match state.get_mut(&key) {
                Some(entry) => {
                    entry.count += 1;
                    if entry.infos.len() < MAX_STORED_PREVIEWS {
                        entry.infos.push(info);
                    }
                    if self.threshold > 0 && entry.count >= self.threshold {
                        let full = state.remove(&key).expect("entry present");
                        Action::FlushNow(full)
                    } else {
                        Action::NoTimer
                    }
                }
                None => {
                    let entry = BatchEntry {
                        opened_at: self.clock.now(),
                        infos: vec![info],
                        count: 1,
                    };
                    state.insert(key.clone(), entry);
                    Action::ArmTimer
                }
            }
        };

        match action {
            Action::ArmTimer => {
                self.timer_spawns.fetch_add(1, Ordering::Relaxed);
                self.spawn_window_flush(client, key);
            }
            Action::NoTimer => {}
            Action::FlushNow(entry) => self.deliver_digest(client, &key, &entry),
        }
    }

    /// Spawn the window's single timer task. It sleeps until the fixed
    /// deadline (`opened_at + window`), not a fresh window per push — a
    /// steady trickle below the threshold still flushes at the deadline.
    fn spawn_window_flush(self: &Arc<Self>, client: &Client, key: String) {
        let batcher = Arc::clone(self);
        let client = client.clone();
        let (opened_at, delay) = {
            let state = self.state.lock().expect("notify batcher lock");
            match state.get(&key) {
                Some(entry) => {
                    let opened_at = entry.opened_at;
                    (
                        opened_at,
                        (opened_at + self.window).saturating_duration_since(self.clock.now()),
                    )
                }
                // Threshold-flushed between insert and arm (unit tests drive
                // `push` without yielding): nothing to wait for.
                None => return,
            }
        };
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            batcher.flush_expired(&client, &key, opened_at);
        });
    }

    /// Flush a batch if it is still the same window that spawned this task.
    fn flush_expired(&self, client: &Client, key: &str, opened_at: Instant) {
        let entry = {
            // Scoped guard: released before the (async) delivery below.
            let mut state = self.state.lock().expect("notify batcher lock");
            match state.get(key) {
                Some(e) if e.opened_at == opened_at => state.remove(key).expect("entry present"),
                _ => return, // already flushed (threshold/drain) or a newer window owns the key
            }
        };
        self.deliver_digest(client, key, &entry);
    }

    /// Flush every window whose fixed deadline has passed. The timer tasks
    /// flush one key via `flush_expired`; tests drive this directly with the
    /// fake clock instead of sleeping through real windows.
    #[cfg(test)]
    pub(crate) fn flush_due(&self, client: &Client, now: Instant) {
        let due: Vec<(String, BatchEntry)> = {
            let mut state = self.state.lock().expect("notify batcher lock");
            let keys: Vec<String> = state
                .iter()
                .filter(|(_, entry)| entry.opened_at + self.window <= now)
                .map(|(key, _)| key.clone())
                .collect();
            keys.into_iter()
                .map(|key| {
                    let entry = state.remove(&key).expect("listed key present");
                    (key, entry)
                })
                .collect()
        };
        for (key, entry) in &due {
            self.deliver_digest(client, key, entry);
        }
    }

    /// Flush every open window as a final digest, awaiting delivery on every
    /// channel, then await spawned in-flight sends (immediate-mode has no
    /// windows to flush). Called from the graceful-shutdown path in `main.rs`
    /// after the server stops accepting connections, so a restart during a
    /// burst still alerts the admin. Empty when no window is open and nothing
    /// is in flight. Total time is bounded by [`DRAIN_TIMEOUT`].
    pub async fn drain(&self, client: &Client) {
        let _ = self.drain_bounded(client, DRAIN_TIMEOUT).await;
    }

    /// [`drain`] with an injected bound, so tests can prove boundedness with
    /// a short timeout instead of the production one.
    pub(crate) async fn drain_bounded(&self, client: &Client, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.drain_inner(client)).await;
    }

    async fn drain_inner(&self, client: &Client) {
        let open: Vec<(String, BatchEntry)> = {
            let mut state = self.state.lock().expect("notify batcher lock");
            let keys: Vec<String> = state.keys().cloned().collect();
            keys.into_iter()
                .map(|key| {
                    let entry = state.remove(&key).expect("listed key present");
                    (key, entry)
                })
                .collect()
        };
        for (key, entry) in &open {
            let digest = Digest::from_batch(key, entry);
            deliver_digest_sync(client, &self.notifier, &digest).await;
        }
        // Await detached sends (immediate singles, threshold/window digests)
        // so shutdown does not kill them mid-flight.
        self.notifier.inflight.idle().await;
    }

    /// Send the aggregated digest for a batch to every configured channel.
    fn deliver_digest(&self, client: &Client, key: &str, entry: &BatchEntry) {
        let digest = Digest::from_batch(key, entry);
        if !self.notifier.is_empty() {
            deliver_digest_to_channels(client, &self.notifier, &digest);
        }
    }
}

impl Default for NotificationBatcher {
    /// A no-op batcher: no channels, immediate mode.
    fn default() -> Self {
        Self {
            notifier: Notifier::default(),
            window: Duration::ZERO,
            threshold: 0,
            global: false,
            clock: Arc::new(SystemClock),
            timer_spawns: AtomicU64::new(0),
            state: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use reqwest::Client;

    use super::{FakeClock, NotificationBatcher};
    use crate::config::Config;
    use crate::notify::NewCommentInfo;

    fn comment_on(path: &str, author: &str, content: &str) -> NewCommentInfo {
        NewCommentInfo {
            id: 1,
            target_path: path.to_string(),
            comment_type: "native".to_string(),
            author_name: author.to_string(),
            author_url: None,
            content: content.to_string(),
            honeypot: false,
            is_reply: false,
        }
    }

    fn test_config(
        window_secs: u64,
        threshold: u32,
        granularity: &str,
        api_base: String,
    ) -> Config {
        Config {
            telegram_bot_token: Some("T".to_string()),
            telegram_chat_id: Some("C".to_string()),
            telegram_api_base: api_base,
            notify_batch_secs: window_secs,
            notify_batch_threshold: threshold,
            notify_batch_granularity: granularity.parse().expect("test granularity valid"),
            ..Config::default()
        }
    }

    async fn mount_telegram_ok(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(server)
            .await;
    }

    async fn wait_for(server: &wiremock::MockServer, n: usize) -> Vec<wiremock::Request> {
        for _ in 0..150 {
            let reqs = server.received_requests().await.unwrap_or_default();
            if reqs.len() >= n {
                return reqs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.received_requests().await.unwrap_or_default()
    }

    #[test]
    fn clock_seam_starts_at_injected_time() {
        let start = Instant::now();
        let clock = FakeClock::new(start);
        assert_eq!(
            clock.now(),
            start,
            "fake clock must report the injected instant"
        );
        clock.advance(Duration::from_secs(60));
        assert_eq!(clock.now(), start + Duration::from_secs(60));
    }

    #[tokio::test]
    async fn window_flushes_at_opened_at_plus_window_exactly() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(60, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        batcher.flush_due(
            &client,
            start + Duration::from_secs(60) - Duration::from_millis(1),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "flush before opened_at + window must not deliver"
        );
        // Exactly at the deadline: the window flushes.
        batcher.flush_due(&client, start + Duration::from_secs(60));
        let reqs = wait_for(&server, 1).await;
        assert_eq!(reqs.len(), 1, "window must flush at opened_at + window");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body["text"].as_str().unwrap().contains("1 new comment"),
            "single-comment digest: {}",
            body["text"]
        );
    }

    #[tokio::test]
    async fn trickle_below_threshold_flushes_at_window_end() {
        // Fixed-deadline pin: late comments must not re-arm the window. Two
        // comments 59 s apart with a 60 s window flush together at
        // opened_at + window. (History, corrected: the old per-push timers
        // were effectively fixed-window too — the first timer won — but
        // armed O(N) tasks and the deadline was never pinned by a test.
        // This test pins the deadline against re-arming regressions.)
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(60, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        clock.advance(Duration::from_secs(59));
        batcher.push(&client, comment_on("/p", "Bob", "two"));
        clock.advance(Duration::from_secs(1));
        batcher.flush_due(&client, clock.now());
        let reqs = wait_for(&server, 1).await;
        assert_eq!(
            reqs.len(),
            1,
            "trickle must produce one digest at the fixed deadline"
        );
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("2 new comments"), "both comments: {text}");
        assert!(text.contains("Alice") && text.contains("Bob"), "{text}");
    }

    #[tokio::test]
    async fn one_timer_per_window_not_per_push() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(60, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        for i in 0..5 {
            batcher.push(&client, comment_on("/p", &format!("User{i}"), "hi"));
        }
        assert_eq!(
            batcher.timer_spawns(),
            1,
            "five pushes into one window must arm exactly one timer"
        );
        batcher.flush_due(&client, start + Duration::from_secs(60));
        let reqs = wait_for(&server, 1).await;
        assert_eq!(reqs.len(), 1, "one window = one digest");
    }

    #[tokio::test]
    async fn threshold_flush_mid_window_then_stale_timer_noops() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(3600, 2, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        batcher.push(&client, comment_on("/p", "Bob", "two"));
        let reqs = wait_for(&server, 1).await;
        assert_eq!(reqs.len(), 1, "threshold must flush mid-window");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body["text"].as_str().unwrap().contains("2 new comments"),
            "{}",
            body["text"]
        );
        // The window timer armed at insert is now stale: expiry must not
        // deliver a second digest for the flushed window.
        clock.advance(Duration::from_secs(3600));
        batcher.flush_due(&client, clock.now());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            reqs.len(),
            1,
            "stale timer must no-op after threshold flush"
        );
    }

    #[tokio::test]
    async fn per_page_windows_stay_separate() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(60, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        batcher.push(&client, comment_on("/a", "Alice", "one"));
        batcher.push(&client, comment_on("/b", "Bob", "two"));
        assert_eq!(batcher.timer_spawns(), 2, "one timer per page window");
        batcher.flush_due(&client, start + Duration::from_secs(60));
        let reqs = wait_for(&server, 2).await;
        assert_eq!(reqs.len(), 2, "per-page mode keeps one digest per page");
    }

    #[tokio::test]
    async fn global_granularity_shares_one_window() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(60, 0, "global", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        batcher.push(&client, comment_on("/a", "Alice", "one"));
        batcher.push(&client, comment_on("/b", "Bob", "two"));
        assert_eq!(batcher.timer_spawns(), 1, "global mode arms one timer");
        batcher.flush_due(&client, start + Duration::from_secs(60));
        let reqs = wait_for(&server, 1).await;
        assert_eq!(reqs.len(), 1, "global mode batches both pages");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body["text"].as_str().unwrap().contains("2 new comments"),
            "{}",
            body["text"]
        );
    }

    #[tokio::test]
    async fn immediate_mode_delivers_each_comment() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let config = test_config(0, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::new(&config));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        batcher.push(&client, comment_on("/p", "Bob", "two"));
        let reqs = wait_for(&server, 2).await;
        assert_eq!(reqs.len(), 2, "window 0 delivers every comment");
    }

    #[tokio::test]
    async fn drain_flushes_open_windows_as_final_digests() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let start = Instant::now();
        let clock = Arc::new(FakeClock::new(start));
        let config = test_config(3600, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::with_clock(&config, clock.clone()));
        let client = Client::new();

        batcher.push(&client, comment_on("/a", "Alice", "one"));
        batcher.push(&client, comment_on("/b", "Bob", "two"));
        // Shutdown path: drain before the windows close.
        batcher.drain(&client).await;
        let reqs = wait_for(&server, 2).await;
        assert_eq!(reqs.len(), 2, "drain must flush every open window");
        // The armed timers are stale afterwards: no double delivery.
        clock.advance(Duration::from_secs(3600));
        batcher.flush_due(&client, clock.now());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(reqs.len(), 2, "post-drain timers must no-op");
    }

    #[tokio::test]
    async fn drain_with_no_open_windows_sends_nothing() {
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let config = test_config(60, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::new(&config));
        let client = Client::new();

        batcher.drain(&client).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "empty drain must not send"
        );
    }

    #[test]
    fn drain_timeout_is_bounded_for_shutdown() {
        let bound = super::DRAIN_TIMEOUT;
        assert!(
            Duration::from_secs(10) <= bound && bound <= Duration::from_secs(15),
            "shutdown latency must be bounded to ~10-15 s, got {bound:?}"
        );
    }

    /// Blackhole: accepts connections, reads the request, never responds.
    /// A wedged channel would stall each attempt for the full 10 s client
    /// timeout — the drain bound must cut it short instead.
    async fn blackhole() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    std::future::pending::<()>().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn drain_bounded_by_timeout_against_wedged_channel() {
        let url = blackhole().await;
        let config = test_config(3600, 0, "page", url);
        let batcher = Arc::new(NotificationBatcher::new(&config));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        let start = Instant::now();
        batcher
            .drain_bounded(&client, Duration::from_millis(300))
            .await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "drain must respect its bound, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn immediate_mode_drain_waits_for_in_flight() {
        // The reviewer race: an immediate-mode send is a detached task with
        // no open window, so a windows-only drain returns instantly and the
        // shutdown kills it. Drain must await in-flight sends instead.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(400))
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;
        let config = test_config(0, 0, "page", server.uri());
        let batcher = Arc::new(NotificationBatcher::new(&config));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        let start = Instant::now();
        batcher.drain(&client).await;
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "drain must await the spawned send, returned after {:?}",
            start.elapsed()
        );
        let reqs = server.received_requests().await.unwrap_or_default();
        assert_eq!(reqs.len(), 1, "the in-flight send must complete");
    }

    #[tokio::test]
    async fn real_timer_expiry_delivers_via_spawned_task() {
        // Production-path coverage: a real 100 ms window driven by the
        // actual spawn_window_flush → sleep → flush_expired chain (not the
        // fake-clock flush_due driver). Adds ~150 ms to the suite.
        let server = wiremock::MockServer::start().await;
        mount_telegram_ok(&server).await;
        let config = test_config(3600, 0, "page", server.uri());
        let batcher =
            Arc::new(NotificationBatcher::new(&config).with_window(Duration::from_millis(100)));
        let client = Client::new();

        batcher.push(&client, comment_on("/p", "Alice", "one"));
        let reqs = wait_for(&server, 1).await;
        assert_eq!(reqs.len(), 1, "real timer task must flush the window");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body["text"].as_str().unwrap().contains("1 new comment"),
            "single-comment digest: {}",
            body["text"]
        );
    }
}
