//! Windowed batching of new-comment notifications.
//!
//! Instead of one message per comment, comments are collected per page (or
//! site-wide, see `NotificationBatcher::global`) into time windows. A single
//! aggregated digest is delivered when the window closes, or earlier when the
//! mid-window threshold is reached. In-memory; batches still open at shutdown
//! are lost (same tradeoff as the in-memory rate limiter).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;

use super::{
    Digest, GLOBAL_KEY, MAX_STORED_PREVIEWS, NewCommentInfo, Notifier, deliver_digest_to_channels,
    deliver_new_comment,
};
use crate::config::Config;

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
    state: Mutex<HashMap<String, BatchEntry>>,
}

impl NotificationBatcher {
    pub fn new(config: &Config) -> Self {
        Self {
            notifier: Notifier::new(config),
            window: Duration::from_secs(config.notify_batch_secs),
            threshold: config.notify_batch_threshold as u64,
            global: config.notify_batch_granularity == "global",
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Any channel is configured (regardless of batching mode).
    pub fn has_channels(&self) -> bool {
        !self.notifier.is_empty()
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
            SpawnFlush,
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
                        Action::SpawnFlush
                    }
                }
                None => {
                    let entry = BatchEntry {
                        opened_at: Instant::now(),
                        infos: vec![info],
                        count: 1,
                    };
                    state.insert(key.clone(), entry);
                    Action::SpawnFlush
                }
            }
        };

        match action {
            Action::SpawnFlush => self.spawn_window_flush(client, key),
            Action::FlushNow(entry) => self.deliver_digest(client, &key, &entry),
        }
    }

    /// Spawn a task that flushes the batch when its window closes.
    fn spawn_window_flush(self: &Arc<Self>, client: &Client, key: String) {
        let batcher = Arc::clone(self);
        let client = client.clone();
        let window = self.window;
        let opened_at = {
            let state = self.state.lock().expect("notify batcher lock");
            state
                .get(&key)
                .map(|e| e.opened_at)
                .unwrap_or_else(Instant::now)
        };
        tokio::spawn(async move {
            tokio::time::sleep(window).await;
            batcher.flush_expired(&client, &key, opened_at);
        });
    }

    /// Flush a batch if it is still the same window that spawned this task.
    fn flush_expired(self: &Arc<Self>, client: &Client, key: &str, opened_at: Instant) {
        let entry = {
            // Scoped guard: released before the (async) delivery below.
            let mut state = self.state.lock().expect("notify batcher lock");
            match state.get(key) {
                Some(e) if e.opened_at == opened_at => state.remove(key).expect("entry present"),
                _ => return, // already flushed (threshold) or a newer window owns the key
            }
        };
        self.deliver_digest(client, key, &entry);
    }

    /// Send the aggregated digest for a batch to every configured channel.
    fn deliver_digest(self: &Arc<Self>, client: &Client, key: &str, entry: &BatchEntry) {
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
            state: Mutex::new(HashMap::new()),
        }
    }
}
