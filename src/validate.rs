use url::Url;

#[derive(Debug, Clone, PartialEq)]
pub enum ValidationError {
    MissingLeadingSlash,
    DoubleSlash,
    PathTraversal,
    ControlChars,
    Backslash,
    TooLong { max: usize, got: usize },
    InvalidScheme(String),
    RelativeUrl,
    InvalidUrl(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingLeadingSlash => write!(f, "target_path must start with /"),
            Self::DoubleSlash => write!(f, "target_path must not contain //"),
            Self::PathTraversal => write!(f, "target_path must not contain .."),
            Self::ControlChars => write!(f, "value contains control characters"),
            Self::Backslash => write!(f, "target_path must not contain backslash"),
            Self::TooLong { max, got } => {
                write!(f, "value exceeds max length of {max}, got {got}")
            }
            Self::InvalidScheme(s) => write!(f, "invalid URL scheme: {s}, expected http or https"),
            Self::RelativeUrl => write!(f, "URL must be absolute"),
            Self::InvalidUrl(s) => write!(f, "invalid URL: {s}"),
        }
    }
}

/// Validate that `path` is a safe target_path per SPEC §6.8.
pub fn validate_target_path(path: &str) -> Result<(), ValidationError> {
    if !path.starts_with('/') {
        return Err(ValidationError::MissingLeadingSlash);
    }
    if path.contains("//") {
        return Err(ValidationError::DoubleSlash);
    }
    if path.contains("..") {
        return Err(ValidationError::PathTraversal);
    }
    if path.contains('\\') {
        return Err(ValidationError::Backslash);
    }
    if path.chars().any(|c| c.is_control()) {
        return Err(ValidationError::ControlChars);
    }
    if path.len() > 1024 {
        return Err(ValidationError::TooLong {
            max: 1024,
            got: path.len(),
        });
    }
    Ok(())
}

/// Validate that `url` is an absolute http or https URL with a host.
pub fn validate_http_url(url_str: &str) -> Result<(), ValidationError> {
    let parsed =
        Url::parse(url_str).map_err(|_| ValidationError::InvalidUrl(url_str.to_string()))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(ValidationError::InvalidScheme(parsed.scheme().to_string()));
    }
    Ok(())
}

/// Strip ASCII control characters (U+0000–U+001F, excluding nothing per spec).
pub fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Truncate a string to at most `max_chars` characters (respects char boundaries).
pub fn clamp_to_max_len(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_target_path ──────────────────────────────

    #[test]
    fn rejects_no_leading_slash() {
        assert_eq!(
            validate_target_path("no-slash"),
            Err(ValidationError::MissingLeadingSlash)
        );
    }

    #[test]
    fn rejects_double_slash() {
        assert_eq!(
            validate_target_path("//double"),
            Err(ValidationError::DoubleSlash)
        );
    }

    #[test]
    fn rejects_dotdot() {
        assert_eq!(
            validate_target_path("/../escape"),
            Err(ValidationError::PathTraversal)
        );
    }

    #[test]
    fn rejects_backslash() {
        assert_eq!(
            validate_target_path("/bad\\path"),
            Err(ValidationError::Backslash)
        );
    }

    #[test]
    fn rejects_control_chars() {
        assert_eq!(
            validate_target_path("/bad\x00path"),
            Err(ValidationError::ControlChars)
        );
        assert_eq!(
            validate_target_path("/bad\x01path"),
            Err(ValidationError::ControlChars)
        );
        assert_eq!(
            validate_target_path("/bad\x1Fpath"),
            Err(ValidationError::ControlChars)
        );
    }

    #[test]
    fn rejects_over_1024_chars() {
        let long = "/".to_string() + &"a".repeat(1024);
        let long = long; // &str
        assert_eq!(
            validate_target_path(&long),
            Err(ValidationError::TooLong {
                max: 1024,
                got: 1025
            })
        );
    }

    #[test]
    fn accepts_root() {
        assert!(validate_target_path("/").is_ok());
    }

    #[test]
    fn accepts_normal_path() {
        assert!(validate_target_path("/blog/hello-world").is_ok());
    }

    #[test]
    fn accepts_nested() {
        assert!(validate_target_path("/blog/2026/07/post-title").is_ok());
    }

    // ── validate_http_url ────────────────────────────────

    #[test]
    fn rejects_ftp_scheme() {
        let result = validate_http_url("ftp://example.com/file");
        assert_eq!(
            result,
            Err(ValidationError::InvalidScheme("ftp".to_string()))
        );
    }

    #[test]
    fn rejects_relative_url() {
        let result = validate_http_url("/relative/path");
        assert!(matches!(result, Err(ValidationError::InvalidUrl(_))));
    }

    #[test]
    fn rejects_scheme_without_authority() {
        // data: URLs have no host — rejected via InvalidScheme before any host check.
        let result = validate_http_url("data:text/plain,hello");
        assert!(matches!(result, Err(ValidationError::InvalidScheme(_))));
    }

    #[test]
    fn accepts_https() {
        assert!(validate_http_url("https://example.com").is_ok());
    }

    #[test]
    fn accepts_http() {
        assert!(validate_http_url("http://example.com").is_ok());
    }

    #[test]
    fn accepts_https_with_path() {
        assert!(validate_http_url("https://alice.blog/posts/1").is_ok());
    }

    #[test]
    fn accepts_url_with_port() {
        assert!(validate_http_url("https://example.com:8080/path").is_ok());
    }

    #[test]
    fn rejects_empty_url() {
        let result = validate_http_url("");
        assert!(matches!(result, Err(ValidationError::InvalidUrl(_))));
    }

    #[test]
    fn rejects_garbage() {
        let result = validate_http_url(" not a url ");
        assert!(matches!(result, Err(ValidationError::InvalidUrl(_))));
    }

    // ── strip_control_chars ──────────────────────────────

    #[test]
    fn strips_null_byte() {
        assert_eq!(strip_control_chars("ab\x00cd"), "abcd");
    }

    #[test]
    fn strips_tab_and_newline() {
        // Per spec, newlines are also control chars and should be stripped.
        assert_eq!(strip_control_chars("a\tb\nc"), "abc");
    }

    #[test]
    fn strips_all_ascii_controls() {
        let controls: String = (0u8..=31).map(char::from).collect();
        let input = format!("before{}after", controls);
        assert_eq!(strip_control_chars(&input), "beforeafter");
    }

    #[test]
    fn leaves_normal_text_untouched() {
        assert_eq!(strip_control_chars("Hello, World!"), "Hello, World!");
    }

    #[test]
    fn leaves_unicode_untouched() {
        // Non-ASCII chars like é, 你好 are not control characters.
        assert_eq!(strip_control_chars("é 日本語"), "é 日本語");
    }

    // ── clamp_to_max_len ────────────────────────────────

    #[test]
    fn preserves_short_string() {
        assert_eq!(clamp_to_max_len("hi", 100), "hi");
    }

    #[test]
    fn truncates_long_string() {
        assert_eq!(clamp_to_max_len("hello world", 5), "hello");
    }

    #[test]
    fn truncation_respects_char_boundary() {
        assert_eq!(clamp_to_max_len("abééé", 3), "abé");
    }

    #[test]
    fn empty_returns_empty() {
        assert_eq!(clamp_to_max_len("", 100), "");
    }

    #[test]
    fn zero_max_returns_empty() {
        assert_eq!(clamp_to_max_len("anything", 0), "");
    }
}
