//! Discord channel: incoming-webhook messages in markdown.

use reqwest::Client;
use serde_json::json;

use super::{Digest, NewCommentInfo, by_line};
use crate::notify::strip_html;

/// POST an incoming-webhook payload to Discord (as the `zapiska` bot user).
async fn send(client: &Client, url: &str, content: &str) -> Result<reqwest::StatusCode, String> {
    let resp = client
        .post(url)
        .json(&json!({ "content": content, "username": "zapiska" }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(resp.status())
}

/// Fire-and-forget a message to Discord; failures are logged, never surfaced.
pub(crate) fn spawn(client: &Client, url: &str, content: &str) {
    let client = client.clone();
    let url = url.to_string();
    let content = content.to_string();
    tokio::spawn(async move {
        match send(&client, &url, &content).await {
            Ok(status) => tracing::debug!(status = %status, "discord notification sent"),
            Err(e) => tracing::warn!(err = %e, "discord notification failed"),
        }
    });
}

/// Build the Discord message content for a single comment (markdown).
pub(crate) fn build_single_payload(info: &NewCommentInfo) -> String {
    const MAX_PREVIEW_CHARS: usize = 300;

    let mut lines = vec![format!("**New comment on {}**", info.target_path)];
    if let Some(ref url) = info.author_url {
        lines.push(format!("By: **{}** ({url})", info.author_name));
    } else {
        lines.push(format!("By: **{}**", info.author_name));
    }
    if info.is_reply {
        lines.push("(reply in thread)".to_string());
    }
    if info.honeypot {
        lines.push(":warning: honeypot triggered".to_string());
    }
    let preview: String = strip_html(&info.content)
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect();
    if !preview.trim().is_empty() {
        lines.push(format!("> {preview}"));
    }
    lines.push(format!("Moderate: /api/admin/comments/{}", info.id));
    lines.join("\n")
}

/// Build the Discord message content for a batch digest (markdown).
pub(crate) fn build_digest_payload(d: &Digest) -> String {
    let noun = if d.count == 1 {
        "new comment"
    } else {
        "new comments"
    };
    let mut lines = vec![format!("**{} {} on {}**", d.count, noun, d.page_label())];
    if let Some(by_line) = by_line(d.count, &d.commenters) {
        lines.push(format!("By: {by_line}"));
    }
    for p in d.previews.iter().take(2) {
        let preview = if p.content.trim().is_empty() {
            "(no text)".to_string()
        } else {
            p.content.clone()
        };
        if d.key == super::GLOBAL_KEY {
            lines.push(format!(
                "First: **{}** on {}: \"{}\"",
                p.author, p.target_path, preview
            ));
        } else {
            lines.push(format!("First: **{}**: \"{}\"", p.author, preview));
        }
    }
    lines.push(format!("Moderate: {}", d.admin_path()));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::test_util::sample_digest;

    #[test]
    fn digest_aggregates() {
        let text = build_digest_payload(&sample_digest());
        assert!(text.contains("**6 new comments on /blog/hello**"), "{text}");
        assert!(text.contains("Alice, Bob, Carol"), "{text}");
        assert!(text.contains("Great post!"), "{text}");
        assert!(
            text.contains("/api/admin/pending?path=/blog/hello"),
            "{text}"
        );
    }
}
