//! Telegram channel: Bot API `sendMessage` with HTML parse mode.

use serde_json::json;

use super::channel::{Channel, Outgoing, hard_truncate, shrink_raw};
use super::{Digest, NewCommentInfo, by_line};
use crate::notify::strip_html;

/// Telegram rejects messages longer than 4096 characters. The builders below
/// keep every payload within that budget by shrinking the cheapest fields
/// first (preview text, then author/commenter names), keeping the `Moderate:`
/// footer through every shrink stage. Only the last-resort `hard_truncate` —
/// reached when the unshrunk fields alone exceed the budget — can cut it.
pub(crate) const TELEGRAM_MAX_CHARS: usize = 4096;

/// Telegram adapter: wire format + endpoint only. Retry, timeout, and budget
/// helpers are shared (`super::channel`); dispatch is shared (`super`).
pub(crate) struct TelegramChannel {
    api_base: String,
    token: String,
    chat_id: String,
}

impl TelegramChannel {
    pub(crate) fn new(api_base: String, token: String, chat_id: String) -> Self {
        Self {
            api_base,
            token,
            chat_id,
        }
    }

    fn outgoing(&self, text: String) -> Outgoing {
        Outgoing {
            url: format!("{}/bot{}/sendMessage", self.api_base, self.token),
            body: json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "HTML",
            }),
        }
    }
}

impl Channel for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn max_chars(&self) -> usize {
        TELEGRAM_MAX_CHARS
    }

    fn format_single(&self, info: &NewCommentInfo) -> Outgoing {
        self.outgoing(build_single_text(info))
    }

    fn format_digest(&self, digest: &Digest) -> Outgoing {
        self.outgoing(build_digest_text(digest))
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
/// Shrinks preview/author to the channel budget; small inputs render exactly
/// as before.
pub(crate) fn build_single_text(info: &NewCommentInfo) -> String {
    const MAX_PREVIEW_CHARS: usize = 300;

    let stripped = strip_html(&info.content);
    let truncated = stripped.chars().count() > MAX_PREVIEW_CHARS;
    let mut preview: String = stripped.chars().take(MAX_PREVIEW_CHARS).collect();
    if truncated {
        preview.push_str("...");
    }
    let mut author = info.author_name.clone();

    loop {
        let text = assemble_single(info, &author, &preview);
        if text.chars().count() <= TELEGRAM_MAX_CHARS {
            return text;
        }
        let overflow = text.chars().count() - TELEGRAM_MAX_CHARS;
        if !preview.trim().is_empty() {
            preview = shrink_raw(&preview, overflow);
        } else if !author.trim().is_empty() {
            author = shrink_raw(&author, overflow);
        } else {
            return hard_truncate(&text, TELEGRAM_MAX_CHARS);
        }
    }
}

/// Assemble the single-comment message from raw (unescaped) parts; escaping
/// happens here so budget iterations re-escape the shrunk fields.
fn assemble_single(info: &NewCommentInfo, author: &str, preview: &str) -> String {
    let mut out = String::new();
    out.push_str("<b>New comment on ");
    out.push_str(&escape(&info.target_path));
    out.push_str("</b>\n");

    if let Some(ref url) = info.author_url {
        out.push_str(&format!(
            "By: <a href=\"{}\">{}</a>",
            escape(url),
            escape(author)
        ));
    } else {
        out.push_str(&format!("By: {}", escape(author)));
    }
    out.push('\n');

    if info.is_reply {
        out.push_str("(reply in thread)\n");
    }
    if info.honeypot {
        out.push_str("<i>⚠ honeypot triggered</i>\n");
    }

    if !preview.trim().is_empty() {
        out.push_str(&escape(preview));
        out.push('\n');
    }

    out.push_str(&format!("Moderate: /api/admin/comments/{}", info.id));
    out
}

/// Build the Telegram digest text for a batch (HTML parse mode).
/// Shrinks preview text, then preview authors, then the commenter list to
/// the channel budget; small inputs render exactly as before.
pub(crate) fn build_digest_text(d: &Digest) -> String {
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
        let text = assemble_digest(d, &commenters, &previews);
        if text.chars().count() <= TELEGRAM_MAX_CHARS {
            return text;
        }
        let overflow = text.chars().count() - TELEGRAM_MAX_CHARS;
        if let Some(idx) = super::channel::longest_field(previews.iter().map(|(_, _, c)| c)) {
            previews[idx].2 = shrink_raw(&previews[idx].2, overflow);
        } else if let Some(idx) = super::channel::longest_field(previews.iter().map(|(a, _, _)| a))
        {
            previews[idx].0 = shrink_raw(&previews[idx].0, overflow);
        } else if !commenters.is_empty() {
            commenters.pop();
        } else {
            return hard_truncate(&text, TELEGRAM_MAX_CHARS);
        }
    }
}

/// Assemble the digest message from raw parts; `by_line` joins the names
/// first and the line is escaped as one (the pre-existing Telegram policy).
fn assemble_digest(
    d: &Digest,
    commenters: &[String],
    previews: &[(String, String, String)],
) -> String {
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
    if let Some(by_line) = by_line(d.count, commenters) {
        out.push_str(&format!("By: {}\n", escape(&by_line)));
    }
    for (author, path, content) in previews {
        let preview = if content.trim().is_empty() {
            "(no text)".to_string()
        } else {
            content.clone()
        };
        if d.key == super::GLOBAL_KEY {
            out.push_str(&format!(
                "First: <i>{}</i> on {}: \"{}\"\n",
                escape(author),
                escape(path),
                escape(&preview)
            ));
        } else {
            out.push_str(&format!(
                "First: <i>{}</i>: \"{}\"\n",
                escape(author),
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

    #[test]
    fn channel_seam_limits_are_named() {
        assert_eq!(super::TELEGRAM_MAX_CHARS, 4096);
        let adapter = super::TelegramChannel::new(
            "https://api.telegram.org".to_string(),
            "tok".to_string(),
            "chat".to_string(),
        );
        let seam: &dyn crate::notify::channel::Channel = &adapter;
        assert_eq!(seam.name(), "telegram");
        assert_eq!(seam.max_chars(), 4096);
    }

    #[test]
    fn single_respects_size_budget_with_maximal_inputs() {
        let mut info = sample_info();
        info.author_name = "A".repeat(5000);
        info.author_url = Some(format!("https://example.invalid/{}", "u".repeat(1000)));
        info.content = format!("<p>{}</p>", "x".repeat(5000));
        let text = build_single_text(&info);
        assert!(
            text.chars().count() <= super::TELEGRAM_MAX_CHARS,
            "single payload exceeds budget: {} chars",
            text.chars().count()
        );
        assert!(text.contains("Moderate:"), "footer must survive");
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
        let text = build_digest_text(&d);
        assert!(
            text.chars().count() <= super::TELEGRAM_MAX_CHARS,
            "digest payload exceeds budget: {} chars",
            text.chars().count()
        );
        assert!(text.contains("Moderate:"), "footer must survive");
    }

    #[test]
    fn single_neutralizes_adversarial_content() {
        let mut info = sample_info();
        info.author_name = "@everyone **boss** <co>".to_string();
        info.content =
            "<p>Hello @here ||spoiler|| **bold** `code` <script>alert(1)</script></p>".to_string();
        let text = build_single_text(&info);
        // Telegram HTML mode formats only tags/entities: the policy escapes
        // `&`, `<`, `>`. `**`/`||`/backticks are inert literal text there.
        assert!(!text.contains("<script>"), "raw tag leaks: {text}");
        assert!(!text.contains("<co>"), "angle brackets leak: {text}");
        assert!(text.contains("&lt;co&gt;"), "escaping held: {text}");
        assert!(!text.contains("<p>"), "html stripped from preview: {text}");
    }

    #[tokio::test]
    async fn wiremock_adversarial_content_arrives_escaped() {
        use crate::notify::channel::{Channel, post_with_retry};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
            )
            .mount(&server)
            .await;
        let mut info = sample_info();
        info.author_name = "Alice & Bob <co>".to_string();
        info.content = "<p>Hi @everyone <script>alert(1)</script></p>".to_string();
        let adapter = super::TelegramChannel::new(
            server.uri(),
            "TESTTOKEN123:test-secret".to_string(),
            "@test_alerts".to_string(),
        );
        let outgoing = adapter.format_single(&info);
        assert!(
            outgoing
                .url
                .contains("/botTESTTOKEN123:test-secret/sendMessage"),
            "telegram endpoint shape, got {}",
            outgoing.url
        );
        let client = reqwest::Client::new();
        post_with_retry(&client, &adapter, &outgoing)
            .await
            .expect("telegram mock must accept the payload");
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(!reqs.is_empty(), "telegram mock must receive a request");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let text = body["text"].as_str().unwrap();
        assert!(!text.contains("<script>"), "raw tag on wire: {text}");
        assert!(
            text.contains("Alice &amp; Bob &lt;co&gt;"),
            "escaped author on wire: {text}"
        );
        assert_eq!(body["chat_id"], "@test_alerts");
        assert_eq!(body["parse_mode"], "HTML");
    }
}
