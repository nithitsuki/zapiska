use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use url::Url;

/// Sanitize HTML content with ammonia's default policy, then truncate to `max_len` characters.
pub fn sanitize_html(content: &str, max_len: usize) -> String {
    let cleaned = ammonia::clean(content);
    cleaned.chars().take(max_len).collect()
}

/// Compute a content hash for duplicate/spam detection.
/// Normalizes by stripping all HTML tags, lowercasing, collapsing whitespace,
/// then computing a SipHash (non-cryptographic, fast).
/// Output is a 16-char hex string with an `h:` prefix.
pub fn content_hash(raw_content: &str) -> String {
    // Strip HTML tags via simple regex
    let no_tags = raw_content
        .chars()
        .fold((String::new(), false), |(mut acc, in_tag), c| match c {
            '<' => (acc, true),
            '>' => (acc, false),
            _ if in_tag => (acc, true),
            _ => {
                acc.push(c);
                (acc, false)
            }
        })
        .0;
    let normalized: String = no_tags
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let words: Vec<&str> = normalized.split_whitespace().collect();
    let cleaned = words.join(" ");

    let mut hasher = DefaultHasher::new();
    cleaned.hash(&mut hasher);
    format!("h:{:016x}", hasher.finish())
}

/// Extract URLs from HTML content. Returns (url, domain, url_hash) tuples.
/// The URL is normalized: lowercased, trailing slash preserved, fragment removed.
/// Domain is extracted from the URL host.
/// url_hash is a SipHash of the normalized URL (hex, `h:` prefix).
pub fn extract_urls(html: &str) -> Vec<(String, String, String)> {
    let mut urls = Vec::new();
    let bytes = html.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        // Find the next href=" ... " (case-insensitive) directly on the
        // original bytes. Never lowercases the input: Unicode lowercasing
        // can change byte lengths (e.g. 'İ'), which would shift offsets and
        // let user-controlled content panic the byte slicing.
        if let Some(start) = find_ascii_ci(bytes, b"href=\"", pos) {
            let abs_start = start + 6;
            let remaining = &html[abs_start..];
            if let Some(end) = remaining.find('"') {
                let raw_url = &remaining[..end];
                // Normalize: parse as URL, resolve if needed
                if let Ok(parsed) = Url::parse(raw_url) {
                    if parsed.scheme() == "http" || parsed.scheme() == "https" {
                        let mut normalized = parsed.clone();
                        normalized.set_fragment(None);
                        let url_str = normalized.to_string().to_lowercase();
                        let domain = normalized.host_str().unwrap_or("").to_string();
                        let mut hasher = DefaultHasher::new();
                        url_str.hash(&mut hasher);
                        let url_hash = format!("h:{:016x}", hasher.finish());
                        urls.push((url_str, domain, url_hash));
                    }
                }
                pos = abs_start + end + 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // Deduplicate by url_hash
    let mut seen = std::collections::HashSet::new();
    urls.retain(|(_, _, hash)| seen.insert(hash.clone()));

    urls
}

/// ASCII case-insensitive substring search (no allocation, linear time).
fn find_ascii_ci(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= haystack.len() {
        if haystack[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_tags() {
        let result = sanitize_html("<script>alert('xss')</script><p>hello</p>", 2000);
        assert_eq!(result, "<p>hello</p>");
    }

    #[test]
    fn strips_event_handlers() {
        let result = sanitize_html("<p onclick=\"alert(1)\">click</p>", 2000);
        assert_eq!(result, "<p>click</p>");
    }

    #[test]
    fn strips_style_blocks() {
        let result = sanitize_html("<style>body { color: red; }</style><p>text</p>", 2000);
        assert_eq!(result, "<p>text</p>");
    }

    #[test]
    fn strips_iframes() {
        let result = sanitize_html(
            "<iframe src='http://evil.example'></iframe><p>safe</p>",
            2000,
        );
        assert_eq!(result, "<p>safe</p>");
    }

    #[test]
    fn truncates_to_max_len() {
        let result = sanitize_html("abcdefghij", 5);
        assert_eq!(result, "abcde");
    }

    #[test]
    fn truncation_respects_char_boundary() {
        // Multi-byte character: "é" is 2 bytes, 1 char.
        let result = sanitize_html("abééé", 3);
        assert_eq!(result, "abé");
    }

    #[test]
    fn preserves_safe_html() {
        let input = "<p>Hello <a href='https://example.com'>link</a></p><code>fn()</code><em>italic</em><strong>bold</strong>";
        let result = sanitize_html(input, 2000);
        assert!(result.contains("<p>"));
        assert!(result.contains("<a href"));
        assert!(result.contains("<code>"));
        assert!(result.contains("<em>"));
        assert!(result.contains("<strong>"));
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(sanitize_html("", 2000), "");
    }

    #[test]
    fn no_html_is_left_untouched() {
        assert_eq!(sanitize_html("just plain text", 2000), "just plain text");
    }

    #[test]
    fn xss_svg_onload() {
        let result = sanitize_html("<svg/onload=alert(1)>", 2000);
        assert!(!result.contains("onload"), "svg onload should be stripped");
        // Ammonia may leave <svg> or strip it entirely; just verify onload is gone.
        assert!(!result.contains("alert"));
    }

    #[test]
    fn xss_img_onerror() {
        let result = sanitize_html("<img src=x onerror=alert(1)>", 2000);
        assert!(!result.contains("onerror"));
        assert!(!result.contains("alert"));
    }

    #[test]
    fn xss_javascript_url() {
        let result = sanitize_html("<a href='javascript:alert(1)'>click</a>", 2000);
        // Ammonia strips javascript: scheme from href.
        assert!(
            !result.contains("javascript:"),
            "javascript: scheme stripped"
        );
        assert!(result.contains("click"), "link text preserved");
    }

    #[test]
    fn xss_nested_script_in_div() {
        let result = sanitize_html("<div><script>evil</script></div>", 2000);
        assert_eq!(result, "<div></div>");
    }

    #[test]
    fn xss_encoded_event_handler() {
        // Ammonia deals with standard encoding; this should still be stripped.
        let result = sanitize_html("<p OnLoad=\"alert(1)\">hi</p>", 2000);
        assert!(!result.contains("OnLoad"));
        assert!(!result.contains("alert"));
    }

    #[test]
    fn extract_urls_survives_unicode_before_href() {
        // 'İ' lowercases to two chars — the old offset translation could
        // panic the byte slicing. Must extract normally.
        let html = "<p>İstanbul İstanbul İstanbul</p><a href=\"https://example.com/x\">link</a>";
        let urls = extract_urls(html);
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].0, "https://example.com/x");
    }

    #[test]
    fn extract_urls_matches_uppercase_href() {
        let html = "<a HREF=\"https://example.com/up\">x</a><A href='https://example.com/sq'>y</A>";
        let urls = extract_urls(html);
        assert_eq!(
            urls.len(),
            1,
            "double-quoted href extracted, single-quoted ignored"
        );
        assert_eq!(urls[0].0, "https://example.com/up");
    }

    #[test]
    fn find_ascii_ci_works() {
        assert_eq!(find_ascii_ci(b"xxHREF=\"yy", b"href=\"", 0), Some(2));
        assert_eq!(find_ascii_ci(b"xxhref=\"yy", b"href=\"", 3), None);
        assert_eq!(find_ascii_ci(b"abc", b"href", 0), None);
    }
}
