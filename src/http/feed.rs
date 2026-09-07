//! RSS 2.0 feeds.
//!
//! - `GET /feed.xml` — global feed: approved comments across ALL paths.
//! - `GET /feed.xml?path=/blog/hello` — feed for a single page.
//!
//! XML is hand-rolled (no feed crate): the codebase deliberately keeps a
//! minimal dependency surface. Dates are converted from SQLite's
//! `YYYY-MM-DD HH:MM:SS` (UTC) to RFC 822 for `pubDate`.

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::db::repo::Comment;
use crate::error::AppError;
use crate::state::AppState;
use crate::validate;

/// Max items per feed (matches the read API cap).
const MAX_ITEMS: i64 = 100;
/// Default items per feed.
const DEFAULT_ITEMS: i64 = 50;

#[derive(Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
pub struct FeedQuery {
    /// Restrict the feed to a single page (e.g. /blog/hello).
    /// Omit for the global feed across all pages.
    #[param(example = "/blog/hello")]
    pub path: Option<String>,
    /// Maximum number of items (max 100, default 50).
    #[param(maximum = 100, default = 50)]
    pub limit: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/feed.xml",
    params(FeedQuery),
    responses(
        (status = 200, description = "RSS 2.0 feed of approved comments", body = String),
        (status = 400, description = "Invalid path parameter"),
    ),
    tag = "comments",
)]
pub async fn feed(
    State(state): State<AppState>,
    Query(query): Query<FeedQuery>,
) -> Result<Response, AppError> {
    let limit = query.limit.unwrap_or(DEFAULT_ITEMS).clamp(1, MAX_ITEMS);
    let origin = state
        .config
        .public_target_origin
        .as_str()
        .trim_end_matches('/')
        .to_string();

    let (comments, title, channel_link, description) = match &query.path {
        Some(path) => {
            validate::validate_target_path(path)
                .map_err(|e| AppError::BadRequest(format!("invalid path: {e}")))?;
            let comments = state.repo.list_approved(path, limit, None).await?;
            (
                comments,
                format!("Comments on {path}"),
                format!("{origin}{path}"),
                format!("Latest comments on {path}"),
            )
        }
        None => {
            let comments = state.repo.list_approved_global(limit).await?;
            (
                comments,
                format!("Comments on {origin}"),
                origin.clone(),
                "Latest comments across the site".to_string(),
            )
        }
    };

    let last_build = comments
        .first()
        .map(|c| rfc822(&c.created_at))
        .unwrap_or_else(|| rfc822(&crate::timeutil::now_sqlite()));

    let include_path_in_titles = query.path.is_none();
    let xml = build_feed(
        &comments,
        &title,
        &channel_link,
        &description,
        &last_build,
        include_path_in_titles,
        &origin,
    );

    Ok((
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

/// Build the full RSS 2.0 document. `include_path_in_titles` adds the page
/// path to item titles (global feed); per-page feeds use bare author names.
/// Item links always point at the comments API on `origin`.
fn build_feed(
    comments: &[Comment],
    title: &str,
    channel_link: &str,
    description: &str,
    last_build: &str,
    include_path_in_titles: bool,
    origin: &str,
) -> String {
    let mut out = String::with_capacity(1024 + comments.len() * 512);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<rss version=\"2.0\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n");
    out.push_str("  <channel>\n");
    out.push_str(&format!("    <title>{}</title>\n", xml_escape(title)));
    out.push_str(&format!("    <link>{}</link>\n", xml_escape(channel_link)));
    out.push_str(&format!(
        "    <description>{}</description>\n",
        xml_escape(description)
    ));
    out.push_str(&format!(
        "    <lastBuildDate>{}</lastBuildDate>\n",
        last_build
    ));
    for c in comments {
        out.push_str("    <item>\n");
        let verified_suffix = if c.verified { " ✓" } else { "" };
        let item_title = if include_path_in_titles {
            format!("{}{} on {}", c.author_name, verified_suffix, c.target_path)
        } else {
            format!("{}{}", c.author_name, verified_suffix)
        };
        out.push_str(&format!(
            "      <title>{}</title>\n",
            xml_escape(&item_title)
        ));
        out.push_str(&format!(
            "      <link>{}</link>\n",
            xml_escape(&format!("{origin}/api/comments?path={}", c.target_path))
        ));
        out.push_str(&format!(
            "      <guid isPermaLink=\"false\">comment-{}</guid>\n",
            c.id
        ));
        out.push_str(&format!(
            "      <pubDate>{}</pubDate>\n",
            rfc822(&c.created_at)
        ));
        out.push_str(&format!(
            "      <dc:creator>{}</dc:creator>\n",
            xml_escape(&c.author_name)
        ));
        out.push_str(&format!(
            "      <description>{}</description>\n",
            xml_escape(&c.content)
        ));
        out.push_str("    </item>\n");
    }
    out.push_str("  </channel>\n");
    out.push_str("</rss>\n");
    out
}

/// Escape a string for XML text content (`&`, `<`, `>`, `"`, `'`).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Convert SQLite `YYYY-MM-DD HH:MM:SS` (UTC) to RFC 822
/// (`Fri, 07 Aug 2026 12:38:11 +0000`). Unparseable input falls back to the
/// Unix epoch so a malformed row can never break the feed.
fn rfc822(sqlite_ts: &str) -> String {
    let bytes = sqlite_ts.as_bytes();
    let parse_ok = bytes.len() >= 19
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !parse_ok {
        return "Thu, 01 Jan 1970 00:00:00 +0000".to_string();
    }

    let y = slice_int(bytes, 0, 4) as i64;
    let m = slice_int(bytes, 5, 7) as i64;
    let d = slice_int(bytes, 8, 10) as i64;
    let hh = slice_int(bytes, 11, 13);
    let mm = slice_int(bytes, 14, 16);
    let ss = slice_int(bytes, 17, 19);
    let (y, m, d, hh, mm, ss) =
        if (1..=12).contains(&m) && (1..=31).contains(&d) && hh < 24 && mm < 60 && ss < 60 {
            (y, m, d, hh, mm, ss)
        } else {
            return "Thu, 01 Jan 1970 00:00:00 +0000".to_string();
        };

    let days = crate::timeutil::days_from_civil(y, m, d);
    let weekday = ((days + 4).rem_euclid(7)) as usize; // 1970-01-01 was a Thursday
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} +0000",
        WEEKDAYS[weekday],
        d,
        MONTHS[(m - 1) as usize],
        y,
        hh,
        mm,
        ss
    )
}

/// Parse a digit slice as an integer. Non-digit bytes contribute nothing
/// (the caller's range validation catches the malformed result).
fn slice_int(bytes: &[u8], start: usize, end: usize) -> u32 {
    bytes[start..end].iter().fold(0u32, |acc, b| {
        if b.is_ascii_digit() {
            acc * 10 + u32::from(b - b'0')
        } else {
            acc
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_comment(id: i64, path: &str, author: &str, content: &str, created: &str) -> Comment {
        Comment {
            id,
            target_path: path.to_string(),
            comment_type: "native".to_string(),
            source_url: None,
            author_name: author.to_string(),
            author_url: None,
            author_avatar: None,
            content: content.to_string(),
            status: "approved".to_string(),
            created_at: created.to_string(),
            updated_at: created.to_string(),
            parent_id: None,
            depth: 0,
            honeypot: false,
            delete_token: None,
            submitter_ip: None,
            submitter_ip_hash: None,
            content_hash: None,
            verified: false,
        }
    }

    #[test]
    fn rfc822_formats_valid_timestamps() {
        assert_eq!(
            rfc822("2026-08-07 12:38:11"),
            "Fri, 07 Aug 2026 12:38:11 +0000"
        );
        assert_eq!(
            rfc822("1970-01-01 00:00:00"),
            "Thu, 01 Jan 1970 00:00:00 +0000"
        );
        assert_eq!(
            rfc822("2024-02-29 23:59:59"),
            "Thu, 29 Feb 2024 23:59:59 +0000"
        );
    }

    #[test]
    fn rfc822_falls_back_on_malformed_input() {
        assert_eq!(rfc822(""), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(rfc822("garbage"), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc822("2026-13-40 99:99:99"),
            "Thu, 01 Jan 1970 00:00:00 +0000"
        );
    }

    #[test]
    fn xml_escape_escapes_all_special_chars() {
        assert_eq!(
            xml_escape("a & b < c > d \"q\" 's'"),
            "a &amp; b &lt; c &gt; d &quot;q&quot; &apos;s&apos;"
        );
    }

    #[test]
    fn feed_structure_contains_expected_elements() {
        let comments = vec![
            sample_comment(
                42,
                "/blog/hello",
                "Alice & Bob",
                "<p>Great <b>post</b>!</p>",
                "2026-08-07 12:38:11",
            ),
            sample_comment(41, "/blog/hello", "Carol", "Second", "2026-08-06 09:00:00"),
        ];
        let xml = build_feed(
            &comments,
            "Comments on /blog/hello",
            "https://nithitsuki.com/blog/hello",
            "Latest comments on /blog/hello",
            "Fri, 07 Aug 2026 12:38:11 +0000",
            false,
            "https://nithitsuki.com",
        );
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<rss version=\"2.0\""));
        assert!(xml.contains("xmlns:dc=\"http://purl.org/dc/elements/1.1/\""));
        assert!(xml.contains("<title>Comments on /blog/hello</title>"));
        assert!(
            xml.contains("<title>Alice &amp; Bob</title>"),
            "bare author title in per-page feed: {xml}"
        );
        assert!(xml.contains("<dc:creator>Alice &amp; Bob</dc:creator>"));
        assert!(
            xml.contains("&lt;p&gt;Great &lt;b&gt;post&lt;/b&gt;!&lt;/p&gt;"),
            "content HTML escaped: {xml}"
        );
        assert!(xml.contains("<guid isPermaLink=\"false\">comment-42</guid>"));
        assert!(xml.contains("<pubDate>Fri, 07 Aug 2026 12:38:11 +0000</pubDate>"));
        assert!(xml.ends_with("</rss>\n"));
    }

    #[test]
    fn feed_empty_channel_still_valid() {
        let xml = build_feed(
            &[],
            "Comments on https://nithitsuki.com",
            "https://nithitsuki.com",
            "Latest comments across the site",
            "Thu, 01 Jan 1970 00:00:00 +0000",
            false,
            "https://nithitsuki.com",
        );
        assert!(xml.contains("<channel>"));
        assert!(xml.contains("</channel>"));
        assert!(!xml.contains("<item>"));
    }

    #[test]
    fn global_feed_titles_include_path() {
        let comments = vec![sample_comment(
            1,
            "/blog/one",
            "Alice",
            "hi",
            "2026-08-07 12:00:00",
        )];
        let xml = build_feed(
            &comments,
            "Comments on https://nithitsuki.com",
            "https://nithitsuki.com",
            "Latest comments across the site",
            "Fri, 07 Aug 2026 12:38:11 +0000",
            true,
            "https://nithitsuki.com",
        );
        // Per-page feeds use bare author names; global feeds add the path.
        assert!(xml.contains("<title>Alice on /blog/one</title>"), "{xml}");
    }
}
