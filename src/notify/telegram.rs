//! Telegram channel: Bot API `sendMessage` with HTML parse mode.

use reqwest::Client;
use serde_json::json;

use super::{Digest, NewCommentInfo, Notifier, by_line};
use crate::notify::strip_html;

/// POST a message to `{api_base}/bot{token}/sendMessage` with HTML parse mode.
async fn send(
    client: &Client,
    api_base: &str,
    token: &str,
    chat_id: &str,
    text: &str,
) -> Result<reqwest::StatusCode, String> {
    let url = format!("{api_base}/bot{token}/sendMessage");
    let resp = client
        .post(&url)
        .json(&json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid response: {e}"))?;
    if body.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(format!("API error: {body}"));
    }
    Ok(status)
}

/// Fire-and-forget a message to Telegram; failures are logged, never surfaced.
pub(crate) fn spawn(client: &Client, notifier: &Notifier, token: &str, chat_id: &str, text: &str) {
    let client = client.clone();
    let api_base = notifier.telegram_api_base.clone();
    let token = token.to_string();
    let chat_id = chat_id.to_string();
    let text = text.to_string();
    tokio::spawn(async move {
        match send(&client, &api_base, &token, &chat_id, &text).await {
            Ok(status) => tracing::debug!(status = %status, "telegram notification sent"),
            Err(e) => tracing::warn!(err = %e, "telegram notification failed"),
        }
    });
}

/// Escape a string for Telegram's HTML parse mode (`&`, `<`, `>`).
pub(crate) fn escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            _ => vec![c],
        })
        .collect()
}

/// Build the Telegram notification text for a single comment (HTML parse mode).
pub(crate) fn build_single_text(info: &NewCommentInfo) -> String {
    const MAX_PREVIEW_CHARS: usize = 300;

    let mut out = String::new();
    out.push_str("<b>New comment on ");
    out.push_str(&escape(&info.target_path));
    out.push_str("</b>\n");

    let author = if let Some(ref url) = info.author_url {
        format!(
            "By: <a href=\"{}\">{}</a>",
            escape(url),
            escape(&info.author_name)
        )
    } else {
        format!("By: {}", escape(&info.author_name))
    };
    out.push_str(&author);
    out.push('\n');

    if info.is_reply {
        out.push_str("(reply in thread)\n");
    }
    if info.honeypot {
        out.push_str("<i>⚠ honeypot triggered</i>\n");
    }

    let stripped = strip_html(&info.content);
    let truncated = stripped.chars().count() > MAX_PREVIEW_CHARS;
    let preview: String = stripped.chars().take(MAX_PREVIEW_CHARS).collect();
    if !preview.trim().is_empty() {
        out.push_str(&escape(&preview));
        if truncated {
            out.push_str("...");
        }
        out.push('\n');
    }

    out.push_str(&format!("Moderate: /api/admin/comments/{}", info.id));
    out
}

/// Build the Telegram digest text for a batch (HTML parse mode).
pub(crate) fn build_digest_text(d: &Digest) -> String {
    let mut out = String::new();
    let noun = if d.count == 1 {
        "new comment"
    } else {
        "new comments"
    };
    out.push_str(&format!(
        "<b>{} {} on {}</b>\n",
        d.count,
        noun,
        escape(d.page_label())
    ));
    if let Some(by_line) = by_line(d.count, &d.commenters) {
        out.push_str(&format!("By: {}\n", escape(&by_line)));
    }
    for p in d.previews.iter().take(2) {
        let preview = if p.content.trim().is_empty() {
            "(no text)".to_string()
        } else {
            p.content.clone()
        };
        if d.key == super::GLOBAL_KEY {
            out.push_str(&format!(
                "First: <i>{}</i> on {}: \"{}\"\n",
                escape(&p.author),
                escape(&p.target_path),
                escape(&preview)
            ));
        } else {
            out.push_str(&format!(
                "First: <i>{}</i>: \"{}\"\n",
                escape(&p.author),
                escape(&preview)
            ));
        }
    }
    out.push_str(&format!("Moderate: {}", escape(&d.admin_path())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::test_util::{sample_digest, sample_info};

    #[test]
    fn escape_escapes_special_chars() {
        assert_eq!(escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn single_text_escapes_html() {
        let text = build_single_text(&sample_info());
        assert!(text.contains("Alice &amp; Bob &lt;co&gt;"), "{text}");
        assert!(!text.contains("<p>"), "html stripped from preview");
    }

    #[test]
    fn digest_aggregates() {
        let text = build_digest_text(&sample_digest());
        assert!(text.contains("6 new comments on /blog/hello"), "{text}");
        assert!(text.contains("Alice, Bob, Carol"), "commenters listed");
        assert!(text.contains("+3 more"), "overflow listed");
        assert!(text.contains("Great post!"), "preview present");
        assert!(
            text.contains("/api/admin/pending?path=/blog/hello"),
            "admin queue path: {text}"
        );
    }

    #[test]
    fn digest_singular_noun() {
        let mut d = sample_digest();
        d.count = 1;
        let text = build_digest_text(&d);
        assert!(text.contains("1 new comment on"), "{text}");
    }
}
