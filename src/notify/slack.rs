//! Slack channel: incoming-webhook payloads in blocks format (mrkdwn).

use reqwest::Client;
use serde_json::json;

use super::{Digest, NewCommentInfo, by_line};
use crate::notify::strip_html;

/// POST an incoming-webhook payload to Slack.
async fn send(
    client: &Client,
    url: &str,
    payload: &serde_json::Value,
) -> Result<reqwest::StatusCode, String> {
    let resp = client
        .post(url)
        .json(payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(resp.status())
}

/// Fire-and-forget a payload to Slack; failures are logged, never surfaced.
pub(crate) fn spawn(client: &Client, url: &str, payload: &serde_json::Value) {
    let client = client.clone();
    let url = url.to_string();
    let payload = payload.clone();
    tokio::spawn(async move {
        match send(&client, &url, &payload).await {
            Ok(status) => tracing::debug!(status = %status, "slack notification sent"),
            Err(e) => tracing::warn!(err = %e, "slack notification failed"),
        }
    });
}

/// Escape a string for Slack mrkdwn text (`&`, `<`, `>`).
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build the Slack incoming-webhook payload for a single comment (blocks).
pub(crate) fn build_single_payload(info: &NewCommentInfo) -> serde_json::Value {
    const MAX_PREVIEW_CHARS: usize = 300;

    let preview: String = strip_html(&info.content)
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect();
    let mut lines = vec![format!("*New comment on `{}`*", escape(&info.target_path))];
    if let Some(ref url) = info.author_url {
        lines.push(format!(
            "*By:* <{}|{}>",
            escape(url),
            escape(&info.author_name)
        ));
    } else {
        lines.push(format!("*By:* {}", escape(&info.author_name)));
    }
    if info.is_reply {
        lines.push("(reply in thread)".to_string());
    }
    if info.honeypot {
        lines.push(":warning: honeypot triggered".to_string());
    }
    if !preview.trim().is_empty() {
        lines.push(format!(">{}", escape(&preview)));
    }

    json!({
        "blocks": [
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": lines.join("\n") }
            },
            {
                "type": "context",
                "elements": [
                    { "type": "mrkdwn", "text": format!(
                        "Comment #{} · {} · Moderate: /api/admin/comments/{}",
                        info.id, info.comment_type, info.id
                    ) }
                ]
            }
        ]
    })
}

/// Build the Slack payload for a batch digest.
pub(crate) fn build_digest_payload(d: &Digest) -> serde_json::Value {
    let noun = if d.count == 1 {
        "new comment"
    } else {
        "new comments"
    };
    let mut lines = vec![format!(
        "*{} {} on `{}`*",
        d.count,
        noun,
        escape(d.page_label())
    )];
    if let Some(by_line) = by_line(d.count, &d.commenters) {
        lines.push(format!("*By:* {}", escape(&by_line)));
    }
    for p in d.previews.iter().take(2) {
        let preview = if p.content.trim().is_empty() {
            "(no text)".to_string()
        } else {
            p.content.clone()
        };
        if d.key == super::GLOBAL_KEY {
            lines.push(format!(
                "*First:* {} on `{}`: \"{}\"",
                escape(&p.author),
                escape(&p.target_path),
                escape(&preview)
            ));
        } else {
            lines.push(format!(
                "*First:* {}: \"{}\"",
                escape(&p.author),
                escape(&preview)
            ));
        }
    }

    json!({
        "blocks": [
            {
                "type": "section",
                "text": { "type": "mrkdwn", "text": lines.join("\n") }
            },
            {
                "type": "context",
                "elements": [
                    { "type": "mrkdwn", "text": format!(
                        "Collected over {} · Moderate: {}",
                        d.window_label(),
                        escape(&d.admin_path())
                    ) }
                ]
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::test_util::{sample_digest, sample_info};

    #[test]
    fn single_payload_structure() {
        let payload = build_single_payload(&sample_info());
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(text.contains("/blog/hello"));
        assert!(text.contains("Alice &amp; Bob &lt;co&gt;"));
        let context = payload["blocks"][1]["elements"][0]["text"]
            .as_str()
            .unwrap();
        assert!(context.contains("Comment #42"));
        assert!(context.contains("Moderate: /api/admin/comments/42"));
    }

    #[test]
    fn digest_aggregates() {
        let payload = build_digest_payload(&sample_digest());
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(text.contains("6 new comments on `/blog/hello`"), "{text}");
        assert!(text.contains("Alice, Bob, Carol"), "{text}");
        let context = payload["blocks"][1]["elements"][0]["text"]
            .as_str()
            .unwrap();
        assert!(
            context.contains("/api/admin/pending?path=/blog/hello"),
            "{context}"
        );
    }
}
