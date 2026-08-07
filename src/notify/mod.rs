//! Admin notification channels (Telegram / Slack / Discord) for new comments.
//!
//! Two delivery modes:
//! - **Immediate** (`NOTIFY_BATCH_SECS = 0`): every new comment is sent to
//!   each configured channel as it arrives.
//! - **Batched** (default): comments are collected into per-page (or global)
//!   windows. When a window closes, ONE aggregated digest message is sent per
//!   channel — the "10+, 100+, 1k+" pattern — so a comment flood produces a
//!   handful of messages, not hundreds.
//!
//! All delivery is fire-and-forget: failures are logged and never affect the
//! comment submission itself.
//!
//! Layout:
//! - [`Notifier`] — channel configuration (which channels are enabled).
//! - [`NotificationBatcher`] — window/threshold batching (see `batcher.rs`).
//! - [`Digest`] — aggregated view of a batch, shared by all channel
//!   formatters.
//! - Per-channel modules (`telegram.rs`, `slack.rs`, `discord.rs`) own their
//!   wire format and message builders.

mod batcher;
mod discord;
mod slack;
mod telegram;

pub use batcher::NotificationBatcher;

use reqwest::Client;

use crate::config::Config;

// ── Configuration ────────────────────────────────────────────

/// Notification targets for new-comment alerts, configured at startup.
#[derive(Debug, Clone)]
pub struct Notifier {
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    /// Base URL of the Telegram Bot API. Defaults to `https://api.telegram.org`;
    /// overridable for tests and proxies.
    pub telegram_api_base: String,
    pub slack_webhook_url: Option<String>,
    pub discord_webhook_url: Option<String>,
}

impl Notifier {
    pub fn new(config: &Config) -> Self {
        Self {
            telegram_bot_token: config.telegram_bot_token.clone(),
            telegram_chat_id: config.telegram_chat_id.clone(),
            telegram_api_base: config.telegram_api_base.clone(),
            slack_webhook_url: config.slack_webhook_url.clone(),
            discord_webhook_url: config.discord_webhook_url.clone(),
        }
    }

    /// No notification channel is fully configured (Telegram needs both
    /// a bot token and a chat ID; Slack and Discord need their webhook URLs).
    pub fn is_empty(&self) -> bool {
        let telegram_ready = self.telegram_bot_token.is_some() && self.telegram_chat_id.is_some();
        !telegram_ready && self.slack_webhook_url.is_none() && self.discord_webhook_url.is_none()
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self {
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_api_base: "https://api.telegram.org".to_string(),
            slack_webhook_url: None,
            discord_webhook_url: None,
        }
    }
}

/// Info about a newly created comment, used to build notification messages.
#[derive(Debug, Clone)]
pub struct NewCommentInfo {
    pub id: i64,
    pub target_path: String,
    pub comment_type: String,
    pub author_name: String,
    pub author_url: Option<String>,
    /// Sanitized HTML content (ammonia-cleaned).
    pub content: String,
    pub honeypot: bool,
    pub is_reply: bool,
}

// ── Digests ──────────────────────────────────────────────────

/// Cap on how many comment previews are kept per batch (for digest messages).
pub(crate) const MAX_STORED_PREVIEWS: usize = 3;
/// Cap on commenter names listed in a digest.
pub(crate) const MAX_LISTED_COMMENTERS: usize = 8;
/// Content preview length for digest messages.
pub(crate) const DIGEST_PREVIEW_CHARS: usize = 200;
/// Key for a site-wide (global granularity) batch.
pub(crate) const GLOBAL_KEY: &str = "__global__";

/// Aggregated view of a batch, shared by all channel formatters.
#[derive(Debug, Clone)]
pub(crate) struct Digest {
    /// Page path, or a site-wide label for global granularity.
    pub(crate) key: String,
    pub(crate) count: u64,
    /// Up to `MAX_LISTED_COMMENTERS` author names.
    pub(crate) commenters: Vec<String>,
    /// Up to `MAX_STORED_PREVIEWS` (author, path, content) samples.
    pub(crate) previews: Vec<DigestPreview>,
}

#[derive(Debug, Clone)]
pub(crate) struct DigestPreview {
    pub(crate) author: String,
    pub(crate) target_path: String,
    pub(crate) content: String,
}

impl Digest {
    pub(crate) fn from_batch(key: &str, entry: &batcher::BatchEntry) -> Self {
        let mut commenters: Vec<String> = Vec::new();
        for info in &entry.infos {
            if !commenters.contains(&info.author_name) {
                commenters.push(info.author_name.clone());
            }
            if commenters.len() >= MAX_LISTED_COMMENTERS {
                break;
            }
        }
        let previews = entry
            .infos
            .iter()
            .take(MAX_STORED_PREVIEWS)
            .map(|info| DigestPreview {
                author: info.author_name.clone(),
                target_path: info.target_path.clone(),
                content: strip_html(&info.content)
                    .chars()
                    .take(DIGEST_PREVIEW_CHARS)
                    .collect(),
            })
            .collect();
        Self {
            key: key.to_string(),
            count: entry.count,
            commenters,
            previews,
        }
    }

    pub(crate) fn page_label(&self) -> &str {
        if self.key == GLOBAL_KEY {
            "the site"
        } else {
            &self.key
        }
    }

    pub(crate) fn admin_path(&self) -> String {
        if self.key == GLOBAL_KEY {
            "/api/admin/pending".to_string()
        } else {
            format!("/api/admin/pending?path={}", self.key)
        }
    }

    /// Human-readable window description for message footers.
    pub(crate) fn window_label(&self) -> String {
        String::from("batching window")
    }
}

// ── Delivery dispatch ────────────────────────────────────────

/// Send one comment immediately to every configured channel (no batching).
pub(crate) fn deliver_new_comment(client: &Client, notifier: &Notifier, info: &NewCommentInfo) {
    if let (Some(token), Some(chat_id)) = (&notifier.telegram_bot_token, &notifier.telegram_chat_id)
    {
        telegram::spawn(
            client,
            notifier,
            token,
            chat_id,
            &telegram::build_single_text(info),
        );
    }
    if let Some(url) = &notifier.slack_webhook_url {
        slack::spawn(client, url, &slack::build_single_payload(info));
    }
    if let Some(url) = &notifier.discord_webhook_url {
        discord::spawn(client, url, &discord::build_single_payload(info));
    }
}

/// Send a batch digest to every configured channel.
pub(crate) fn deliver_digest_to_channels(client: &Client, notifier: &Notifier, digest: &Digest) {
    if let (Some(token), Some(chat_id)) = (&notifier.telegram_bot_token, &notifier.telegram_chat_id)
    {
        telegram::spawn(
            client,
            notifier,
            token,
            chat_id,
            &telegram::build_digest_text(digest),
        );
    }
    if let Some(url) = &notifier.slack_webhook_url {
        slack::spawn(client, url, &slack::build_digest_payload(digest));
    }
    if let Some(url) = &notifier.discord_webhook_url {
        discord::spawn(client, url, &discord::build_digest_payload(digest));
    }
}

// ── Shared helpers ───────────────────────────────────────────

/// "By: Alice, Bob +3 more" line for digests. Returns `None` when there are
/// no commenters to list. `count` may be less than the listed names in
/// contrived cases, so the "+N more" is computed defensively.
pub(crate) fn by_line(count: u64, commenters: &[String]) -> Option<String> {
    if commenters.is_empty() {
        return None;
    }
    let listed = commenters.join(", ");
    let more = count.saturating_sub(commenters.len() as u64);
    if more > 0 {
        Some(format!("{listed} +{more} more"))
    } else {
        Some(listed)
    }
}

/// Strip HTML tags, returning plain text with a space at each tag boundary
/// (so `<p>a</p><p>b</p>` reads as "a b", not "ab").
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                if !in_tag && !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                in_tag = true;
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::{Digest, DigestPreview, NewCommentInfo};

    pub(crate) fn sample_info() -> NewCommentInfo {
        NewCommentInfo {
            id: 42,
            target_path: "/blog/hello".to_string(),
            comment_type: "native".to_string(),
            author_name: "Alice & Bob <co>".to_string(),
            author_url: Some("https://alice.blog".to_string()),
            content: "<p>Great post!</p>".to_string(),
            honeypot: false,
            is_reply: false,
        }
    }

    pub(crate) fn sample_digest() -> Digest {
        Digest {
            key: "/blog/hello".to_string(),
            count: 6,
            commenters: vec!["Alice".to_string(), "Bob".to_string(), "Carol".to_string()],
            previews: vec![
                DigestPreview {
                    author: "Alice".to_string(),
                    target_path: "/blog/hello".to_string(),
                    content: "Great post!".to_string(),
                },
                DigestPreview {
                    author: "Bob".to_string(),
                    target_path: "/blog/hello".to_string(),
                    content: "Second".to_string(),
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{sample_digest, sample_info};
    use super::*;

    #[test]
    fn telegram_text_contains_author_and_path() {
        let text = telegram::build_single_text(&sample_info());
        assert!(text.contains("/blog/hello"), "path in message");
        assert!(
            text.contains("Alice &amp; Bob &lt;co&gt;"),
            "author escaped"
        );
        assert!(text.contains("https://alice.blog"), "author url present");
        assert!(text.contains("Great post!"), "content preview present");
        assert!(
            text.contains("/api/admin/comments/42"),
            "admin path present"
        );
        assert!(!text.contains("<p>"), "html stripped from preview");
    }

    #[test]
    fn telegram_text_marks_honeypot_and_reply() {
        let mut info = sample_info();
        info.honeypot = true;
        info.is_reply = true;
        let text = telegram::build_single_text(&info);
        assert!(text.contains("honeypot triggered"));
        assert!(text.contains("reply in thread"));
    }

    #[test]
    fn telegram_text_truncates_long_content() {
        let mut info = sample_info();
        info.content = format!("<p>{}</p>", "x".repeat(5000));
        let text = telegram::build_single_text(&info);
        assert!(text.chars().count() < 600, "message bounded");
        assert!(text.contains("..."), "truncation marker");
    }

    #[test]
    fn slack_payload_marks_honeypot() {
        let mut info = sample_info();
        info.honeypot = true;
        let payload = slack::build_single_payload(&info);
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(text.contains("honeypot"));
    }

    #[test]
    fn discord_payload_structure() {
        let text = discord::build_single_payload(&sample_info());
        assert!(text.contains("New comment on /blog/hello"));
        assert!(text.contains("Alice & Bob <co>"));
        assert!(text.contains("https://alice.blog"));
        assert!(text.contains("Great post!"));
        assert!(text.contains("/api/admin/comments/42"));
    }

    #[test]
    fn discord_payload_marks_honeypot_and_reply() {
        let mut info = sample_info();
        info.honeypot = true;
        info.is_reply = true;
        let text = discord::build_single_payload(&info);
        assert!(text.contains("honeypot"));
        assert!(text.contains("reply in thread"));
    }

    #[test]
    fn digest_global_label_and_admin_path() {
        let mut d = sample_digest();
        d.key = GLOBAL_KEY.to_string();
        assert_eq!(d.page_label(), "the site");
        assert_eq!(d.admin_path(), "/api/admin/pending");
        let text = discord::build_digest_payload(&d);
        assert!(text.contains("on the site"), "{text}");
    }

    #[test]
    fn notifier_is_empty_logic() {
        assert!(Notifier::default().is_empty());
        let with_slack = Notifier {
            slack_webhook_url: Some("https://hooks.slack.com/services/x".to_string()),
            ..Notifier::default()
        };
        assert!(!with_slack.is_empty());
        let with_telegram = Notifier {
            telegram_bot_token: Some("token".to_string()),
            ..Notifier::default()
        };
        // A token without a chat ID can't send anything.
        assert!(with_telegram.is_empty());
        let with_discord = Notifier {
            discord_webhook_url: Some("https://discord.com/api/webhooks/1/abc".to_string()),
            ..Notifier::default()
        };
        assert!(!with_discord.is_empty());
        let full = Notifier {
            telegram_bot_token: Some("token".to_string()),
            telegram_chat_id: Some("@alerts".to_string()),
            ..Notifier::default()
        };
        assert!(!full.is_empty());
    }

    #[test]
    fn strip_html_removes_tags_and_collapses_whitespace() {
        assert_eq!(strip_html("<p>a</p><p>b</p>"), "a b");
        assert_eq!(strip_html("no tags here"), "no tags here");
        assert_eq!(strip_html(""), "");
    }
}
