/// Sanitize HTML content with ammonia's default policy, then truncate to `max_len` characters.
pub fn sanitize_html(content: &str, max_len: usize) -> String {
    let cleaned = ammonia::clean(content);
    cleaned.chars().take(max_len).collect()
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
}
