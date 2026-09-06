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

/// Discord rejects `content` longer than 2000 characters. The builders below
/// keep every payload within that budget by shrinking the cheapest fields
/// first (preview text, then author/commenter names), keeping the `Moderate:`
/// footer through every shrink stage. Only the last-resort `hard_truncate` —
/// reached when the unshrunk fields alone exceed the budget — can cut it.
pub(crate) const DISCORD_MAX_CHARS: usize = 2000;

/// Escape user-controlled text for Discord markdown.
///
/// Mirrors the `escape` helpers in `telegram.rs`/`slack.rs`, adapted for
/// Discord: every `@` is broken with a zero-width space so `@everyone` and
/// `@here` arrive as inert text, and Discord markdown/formatting characters
/// are backslash-escaped so `**bold**`, `||spoiler||`, `` `code` ``,
/// and `<...>` mention/tag shapes render literally instead of formatting or
/// pinging anyone.
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '*' | '_' | '`' | '~' | '|' | '>' | '<' | '[' | ']' | '(' | ')' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.replace('@', "@\u{200B}")
}

/// Shorten a non-empty already-escaped field by at least `overflow`
/// characters, marking the cut with `...`. Always returns a strictly shorter
/// string, so budget loops terminate.
fn shrink_text(field: &str, overflow: usize) -> String {
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

/// Last-resort cap: cut the whole message to the channel budget. Only reached
/// when every shrinkable field is already empty.
fn hard_truncate(text: &str) -> String {
    if text.chars().count() <= DISCORD_MAX_CHARS {
        return text.to_string();
    }
    text.chars().take(DISCORD_MAX_CHARS - 3).collect::<String>() + "..."
}

/// Index of the longest non-empty string in `fields`, if any.
fn longest_field<'a>(fields: impl Iterator<Item = &'a String>) -> Option<usize> {
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

/// Build the Discord message content for a single comment (markdown).
pub(crate) fn build_single_payload(info: &NewCommentInfo) -> String {
    const MAX_PREVIEW_CHARS: usize = 300;

    let esc_target = escape(&info.target_path);
    let mut esc_author = escape(&info.author_name);
    let esc_url = info.author_url.as_deref().map(escape);
    let raw_preview: String = strip_html(&info.content)
        .chars()
        .take(MAX_PREVIEW_CHARS)
        .collect();
    let mut esc_preview = escape(raw_preview.trim());
    if esc_preview.trim().is_empty() {
        esc_preview = String::new();
    }

    loop {
        let text = assemble_single(
            &esc_target,
            &esc_author,
            esc_url.as_deref(),
            &esc_preview,
            info,
        );
        let len = text.chars().count();
        if len <= DISCORD_MAX_CHARS {
            return text;
        }
        let overflow = len - DISCORD_MAX_CHARS;
        if !esc_preview.is_empty() {
            esc_preview = shrink_text(&esc_preview, overflow);
        } else if !esc_author.is_empty() {
            esc_author = shrink_text(&esc_author, overflow);
        } else {
            return hard_truncate(&text);
        }
    }
}

/// Assemble the single-comment message from already-escaped parts.
fn assemble_single(
    esc_target: &str,
    esc_author: &str,
    esc_url: Option<&str>,
    esc_preview: &str,
    info: &NewCommentInfo,
) -> String {
    let mut lines = vec![format!("**New comment on {esc_target}**")];
    if let Some(url) = esc_url {
        lines.push(format!("By: **{esc_author}** ({url})"));
    } else {
        lines.push(format!("By: **{esc_author}**"));
    }
    if info.is_reply {
        lines.push("(reply in thread)".to_string());
    }
    if info.honeypot {
        lines.push(":warning: honeypot triggered".to_string());
    }
    if !esc_preview.is_empty() {
        lines.push(format!("> {esc_preview}"));
    }
    lines.push(format!("Moderate: /api/admin/comments/{}", info.id));
    lines.join("\n")
}

/// Build the Discord message content for a batch digest (markdown).
pub(crate) fn build_digest_payload(d: &Digest) -> String {
    let esc_page = escape(d.page_label());
    let esc_admin = escape(&d.admin_path());
    let mut commenters: Vec<String> = d.commenters.iter().map(|c| escape(c)).collect();
    let mut previews: Vec<(String, String, String)> = d
        .previews
        .iter()
        .take(2)
        .map(|p| {
            let content = if p.content.trim().is_empty() {
                "(no text)".to_string()
            } else {
                escape(&p.content)
            };
            (escape(&p.author), escape(&p.target_path), content)
        })
        .collect();

    loop {
        let text = assemble_digest(d, &esc_page, &esc_admin, &commenters, &previews);
        let len = text.chars().count();
        if len <= DISCORD_MAX_CHARS {
            return text;
        }
        let overflow = len - DISCORD_MAX_CHARS;
        // Shrink the cheapest fields first: preview text, then preview
        // authors, then the commenter list (`by_line` already renders the
        // remainder as "+N more").
        if let Some(idx) = longest_field(previews.iter().map(|(_, _, c)| c)) {
            previews[idx].2 = shrink_text(&previews[idx].2, overflow);
        } else if let Some(idx) = longest_field(previews.iter().map(|(a, _, _)| a)) {
            previews[idx].0 = shrink_text(&previews[idx].0, overflow);
        } else if !commenters.is_empty() {
            commenters.pop();
        } else {
            return hard_truncate(&text);
        }
    }
}

/// Assemble the digest message from already-escaped parts. `by_line` only
/// joins the pre-escaped names, so no further escaping happens here.
fn assemble_digest(
    d: &Digest,
    esc_page: &str,
    esc_admin: &str,
    commenters: &[String],
    previews: &[(String, String, String)],
) -> String {
    let noun = if d.count == 1 {
        "new comment"
    } else {
        "new comments"
    };
    let mut lines = vec![format!("**{} {noun} on {esc_page}**", d.count)];
    if let Some(by) = by_line(d.count, commenters) {
        lines.push(format!("By: {by}"));
    }
    for (author, path, content) in previews {
        if d.key == super::GLOBAL_KEY {
            lines.push(format!("First: **{author}** on {path}: \"{content}\""));
        } else {
            lines.push(format!("First: **{author}**: \"{content}\""));
        }
    }
    lines.push(format!("Moderate: {esc_admin}"));
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

    #[test]
    fn escape_breaks_everyone_and_here() {
        let out = escape("@everyone hello @here");
        assert!(!out.contains("@everyone"), "raw @everyone leaks: {out}");
        assert!(!out.contains("@here"), "raw @here leaks: {out}");
    }

    #[test]
    fn escape_neutralizes_markdown() {
        let out = escape("**bold** `code` ||spoiler|| _it_ ~~s~~ >q <script> [x](y)");
        assert!(!out.contains("**bold**"), "bold leaks: {out}");
        assert!(!out.contains("`code`"), "code leaks: {out}");
        assert!(!out.contains("||spoiler||"), "spoiler leaks: {out}");
        assert!(!out.contains("<script>"), "tag leaks: {out}");
        assert!(out.contains("\\*"), "star escaped: {out}");
        assert!(out.contains("\\|"), "pipe escaped: {out}");
        assert!(out.contains("\\`"), "backtick escaped: {out}");
    }

    #[test]
    fn single_escapes_attacker_content() {
        let mut info = crate::notify::test_util::sample_info();
        info.author_name = "@everyone **boss**".to_string();
        info.content = "<p>Hello @here ||spoiler|| **bold** `code`</p>".to_string();
        let text = build_single_payload(&info);
        assert!(!text.contains("@everyone"), "mention leaks: {text}");
        assert!(!text.contains("@here"), "mention leaks: {text}");
        assert!(!text.contains("**boss**"), "author markdown leaks: {text}");
        assert!(!text.contains("**bold**"), "content markdown leaks: {text}");
        assert!(!text.contains("||spoiler||"), "spoiler leaks: {text}");
        assert!(!text.contains("`code`"), "code leaks: {text}");
    }

    #[test]
    fn digest_escapes_attacker_content() {
        let mut d = sample_digest();
        d.commenters = vec!["@everyone".to_string(), "**boss**".to_string()];
        d.previews[0].author = "@here".to_string();
        d.previews[0].content = "||spoiler|| **bold** `code`".to_string();
        let text = build_digest_payload(&d);
        assert!(!text.contains("@everyone"), "mention leaks: {text}");
        assert!(!text.contains("@here"), "mention leaks: {text}");
        assert!(
            !text.contains("**boss**"),
            "commenter markdown leaks: {text}"
        );
        assert!(!text.contains("**bold**"), "preview markdown leaks: {text}");
        assert!(!text.contains("||spoiler||"), "spoiler leaks: {text}");
        assert!(!text.contains("`code`"), "code leaks: {text}");
    }

    #[test]
    fn single_respects_size_budget() {
        let mut info = crate::notify::test_util::sample_info();
        info.author_name = "A".repeat(5000);
        info.content = format!("<p>{}</p>", "x".repeat(5000));
        let text = build_single_payload(&info);
        assert!(
            text.chars().count() <= DISCORD_MAX_CHARS,
            "single payload exceeds budget: {} chars",
            text.chars().count()
        );
        assert!(text.contains("Moderate:"), "footer must survive");
    }

    #[test]
    fn digest_respects_size_budget() {
        let mut d = sample_digest();
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
        let text = build_digest_payload(&d);
        assert!(
            text.chars().count() <= DISCORD_MAX_CHARS,
            "digest payload exceeds budget: {} chars",
            text.chars().count()
        );
        assert!(text.contains("Moderate:"), "footer must survive");
    }

    #[tokio::test]
    async fn wiremock_everyone_neutralized() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let mut info = crate::notify::test_util::sample_info();
        info.author_name = "@everyone".to_string();
        info.content = "<p>hi @everyone @here</p>".to_string();
        let payload = build_single_payload(&info);
        let client = Client::new();
        send(&client, &server.uri(), &payload).await.unwrap();
        let reqs = server.received_requests().await.unwrap_or_default();
        assert!(!reqs.is_empty(), "discord mock must receive a request");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let content = body["content"].as_str().unwrap();
        assert!(
            !content.contains("@everyone"),
            "raw mention on wire: {content}"
        );
        assert!(!content.contains("@here"), "raw mention on wire: {content}");
    }
}
