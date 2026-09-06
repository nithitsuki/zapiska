//! Slack channel: incoming-webhook payloads in blocks format (mrkdwn).

use serde_json::{Value, json};

use super::channel::{Channel, Outgoing, hard_truncate, shrink_raw};
use super::{Digest, NewCommentInfo, by_line};
use crate::notify::strip_html;

/// Slack cuts a block's mrkdwn text at 3000 characters. The builders below
/// keep the section text within that budget by shrinking the cheapest fields
/// first (preview text, then author/commenter names); the context footer is a
/// separate block and always survives. Only the last-resort `hard_truncate` —
/// reached when the unshrunk fields alone exceed the budget — can cut the
/// section text itself.
pub(crate) const SLACK_MAX_CHARS: usize = 3000;

/// Slack adapter: wire format + endpoint only. Retry, timeout, and budget
/// helpers are shared (`super::channel`); dispatch is shared (`super`).
pub(crate) struct SlackChannel {
    webhook_url: String,
}

impl SlackChannel {
    pub(crate) fn new(webhook_url: String) -> Self {
        Self { webhook_url }
    }
}

impl Channel for SlackChannel {
    fn name(&self) -> &'static str {
        "slack"
    }

    fn max_chars(&self) -> usize {
        SLACK_MAX_CHARS
    }

    fn format_single(&self, info: &NewCommentInfo) -> Outgoing {
        Outgoing {
            url: self.webhook_url.clone(),
            body: build_single_payload(info),
        }
    }

    fn format_digest(&self, digest: &Digest) -> Outgoing {
        Outgoing {
            url: self.webhook_url.clone(),
            body: build_digest_payload(digest),
        }
    }
}

/// Section text of a blocks payload (the budgeted field).
fn section_text(payload: &Value) -> &str {
    payload["blocks"][0]["text"]["text"].as_str().unwrap_or("")
}

/// Escape a string for Slack mrkdwn text (`&`, `<`, `>`).
pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build the Slack incoming-webhook payload for a single comment (blocks).
/// Shrinks preview/author to the section budget; small inputs render exactly
/// as before.
pub(crate) fn build_single_payload(info: &NewCommentInfo) -> serde_json::Value {
    const MAX_PREVIEW_CHARS: usize = 300;

    let mut preview: String = strip_html(&info.content)
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect();
    let mut author = info.author_name.clone();

    loop {
        let payload = assemble_single(info, &author, &preview);
        if section_text(&payload).chars().count() <= SLACK_MAX_CHARS {
            return payload;
        }
        let overflow = section_text(&payload).chars().count() - SLACK_MAX_CHARS;
        if !preview.trim().is_empty() {
            preview = shrink_raw(&preview, overflow);
        } else if !author.trim().is_empty() {
            author = shrink_raw(&author, overflow);
        } else {
            return cap_section(payload);
        }
    }
}

/// Assemble the single-comment payload from raw (unescaped) parts; escaping
/// happens here so budget iterations re-escape the shrunk fields.
fn assemble_single(info: &NewCommentInfo, author: &str, preview: &str) -> serde_json::Value {
    let mut lines = vec![format!("*New comment on `{}`*", escape(&info.target_path))];
    if let Some(ref url) = info.author_url {
        lines.push(format!("*By:* <{}|{}>", escape(url), escape(author)));
    } else {
        lines.push(format!("*By:* {}", escape(author)));
    }
    if info.is_reply {
        lines.push("(reply in thread)".to_string());
    }
    if info.honeypot {
        lines.push(":warning: honeypot triggered".to_string());
    }
    if !preview.trim().is_empty() {
        lines.push(format!(">{}", escape(preview)));
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

/// Last resort: cap an over-budget section in place, keeping the JSON shape
/// (and the context footer) intact.
fn cap_section(mut payload: Value) -> Value {
    let capped = hard_truncate(section_text(&payload), SLACK_MAX_CHARS);
    payload["blocks"][0]["text"]["text"] = Value::String(capped);
    payload
}

/// Build the Slack payload for a batch digest.
/// Shrinks preview text, then preview authors, then the commenter list to
/// the section budget; small inputs render exactly as before.
pub(crate) fn build_digest_payload(d: &Digest) -> serde_json::Value {
    let mut commenters: Vec<String> = d.commenters.clone();
    let mut previews: Vec<(String, String, String)> = d
        .previews
        .iter()
        .take(2)
        .map(|p| {
            let content = if p.content.trim().is_empty() {
                "(no text)".to_string()
            } else {
                p.content.clone()
            };
            (p.author.clone(), p.target_path.clone(), content)
        })
        .collect();

    loop {
        let payload = assemble_digest(d, &commenters, &previews);
        if section_text(&payload).chars().count() <= SLACK_MAX_CHARS {
            return payload;
        }
        let overflow = section_text(&payload).chars().count() - SLACK_MAX_CHARS;
        if let Some(idx) = super::channel::longest_field(previews.iter().map(|(_, _, c)| c)) {
            previews[idx].2 = shrink_raw(&previews[idx].2, overflow);
        } else if let Some(idx) = super::channel::longest_field(previews.iter().map(|(a, _, _)| a))
        {
            previews[idx].0 = shrink_raw(&previews[idx].0, overflow);
        } else if !commenters.is_empty() {
            commenters.pop();
        } else {
            return cap_section(payload);
        }
    }
}

/// Assemble the digest payload from raw parts; escaping happens here (the
/// pre-existing Slack policy: `&`, `<`, `>`).
fn assemble_digest(
    d: &Digest,
    commenters: &[String],
    previews: &[(String, String, String)],
) -> serde_json::Value {
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
    if let Some(by_line) = by_line(d.count, commenters) {
        lines.push(format!("*By:* {}", escape(&by_line)));
    }
    for (author, path, content) in previews {
        let preview = if content.trim().is_empty() {
            "(no text)".to_string()
        } else {
            content.clone()
        };
        if d.key == super::GLOBAL_KEY {
            lines.push(format!(
                "*First:* {} on `{}`: \"{}\"",
                escape(author),
                escape(path),
                escape(&preview)
            ));
        } else {
            lines.push(format!(
                "*First:* {}: \"{}\"",
                escape(author),
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

    #[test]
    fn channel_seam_limits_are_named() {
        assert_eq!(super::SLACK_MAX_CHARS, 3000);
        let adapter = super::SlackChannel::new("https://hooks.slack.com/services/x".to_string());
        let seam: &dyn crate::notify::channel::Channel = &adapter;
        assert_eq!(seam.name(), "slack");
        assert_eq!(seam.max_chars(), 3000);
    }

    #[test]
    fn single_respects_size_budget_with_maximal_inputs() {
        let mut info = sample_info();
        info.author_name = "A".repeat(5000);
        info.content = format!("<p>{}</p>", "x".repeat(5000));
        let payload = build_single_payload(&info);
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(
            text.chars().count() <= super::SLACK_MAX_CHARS,
            "section text exceeds budget: {} chars",
            text.chars().count()
        );
    }

    #[test]
    fn digest_respects_size_budget_with_maximal_inputs() {
        let mut d = sample_digest();
        d.count = 25;
        d.commenters = (0..8)
            .map(|i| format!("Name{i}{}", "B".repeat(500)))
            .collect();
        d.previews = (0..3)
            .map(|i| crate::notify::DigestPreview {
                author: format!("Author{i}{}", "C".repeat(500)),
                target_path: "/blog/hello".to_string(),
                content: "y".repeat(1000),
            })
            .collect();
        let payload = build_digest_payload(&d);
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(
            text.chars().count() <= super::SLACK_MAX_CHARS,
            "section text exceeds budget: {} chars",
            text.chars().count()
        );
        let context = payload["blocks"][1]["elements"][0]["text"]
            .as_str()
            .unwrap();
        assert!(context.contains("Moderate:"), "footer must survive");
    }

    #[test]
    fn single_neutralizes_adversarial_content() {
        let mut info = sample_info();
        info.author_name = "<!here> **boss**".to_string();
        info.content = "<p>Hello <!here> **bold** <script>alert(1)</script></p>".to_string();
        let payload = build_single_payload(&info);
        let text = payload["blocks"][0]["text"]["text"].as_str().unwrap();
        // Slack mrkdwn policy escapes `&`, `<`, `>` (which also neutralizes
        // the `<!here>` directive shape). Assert that policy, not Discord's.
        assert!(!text.contains("<!here>"), "directive leaks: {text}");
        assert!(!text.contains("<script>"), "raw tag leaks: {text}");
    }

    #[tokio::test]
    async fn wiremock_adversarial_content_arrives_escaped() {
        use crate::notify::channel::{Channel, post_with_retry};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let mut info = sample_info();
        info.author_name = "Alice & Bob <co>".to_string();
        info.content = "<p>Hi <!here> <script>alert(1)</script></p>".to_string();
        let adapter = super::SlackChannel::new(server.uri());
        let outgoing = adapter.format_single(&info);
        assert_eq!(outgoing.url, server.uri());
        let client = reqwest::Client::new();
        post_with_retry(&client, &adapter, &outgoing)
            .await
            .expect("slack mock must accept the payload");
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(!reqs.is_empty(), "slack mock must receive a request");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let text = body["blocks"][0]["text"]["text"].as_str().unwrap();
        assert!(!text.contains("<!here>"), "directive on wire: {text}");
        assert!(!text.contains("<script>"), "raw tag on wire: {text}");
        assert!(
            text.contains("Alice &amp; Bob &lt;co&gt;"),
            "escaped author on wire: {text}"
        );
    }
}
