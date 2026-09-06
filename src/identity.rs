//! Author identity normalization: ONE place for author rules shared by the
//! native pipeline, the import restore path, and the future webmention-author
//! path.
//!
//! Placement: crate root (`crate::identity`), not inside `ingress` or
//! `http::admin::data`. Both the live submission path (`ingress`, HTTP) and
//! the storage restore path (`db::repo::restore`, storage) depend on it —
//! putting it under either would force storage to import from HTTP or vice
//! versa. Same precedent as `crate::moderation` (T16) and `crate::validate`.
//!
//! What lives here:
//! - [`clean_author_name`]: strip `Cc` controls AND `Cf`/bidi spoof
//!   characters (U+202E overrides, U+200B zero-width, isolates), then trim.
//! - [`native_author_name`]: live-submission policy (reject empty without a
//!   GitHub fallback, reject over `MAX_AUTHOR_LEN`).
//! - [`import_author_name`]: historical-data policy (reject empty, CLAMP to
//!   `max_author_len` instead of rejecting — a lowered limit must not drop
//!   backup rows; the divergence from native reject is intentional).
//! - GitHub shape checks re-export [`validate::validate_github_username`];
//!   call it BEFORE interpolating into `https://github.com/{name}`.
//!
//! Follow-ups (NOT fixed here): language-gate quarantine tier (B6) and
//! Unicode body-limit parity (B7) stay as documented hard-blocks.

use crate::validate::{ValidationError, validate_github_username};

/// Spoof-capable format characters stripped from author names alongside
/// `Cc` controls. Each entry is a deliberate choice, not a blanket `Cf`
/// strip (which would also eat ZWJ/ZWNJ needed for legitimate emoji
/// sequences and some scripts):
/// - U+202A–U+202E: bidi embeddings/overrides (incl. U+202E RTL override).
/// - U+2066–U+2069: bidi isolates.
/// - U+200B: zero-width space. U+00AD: soft hyphen. U+FEFF: zero-width
///   no-break space / BOM. U+061C: Arabic letter mark. U+180E: Mongolian
///   vowel separator. U+200E–U+200F: LRM/RLM marks.
///
/// Kept on purpose (NOT stripped): U+200C/U+200D (ZWNJ/ZWJ for emoji
/// ligatures and Indic scripts), U+2060 (word joiner), U+00A0 (nbsp).
const SPOOF_CHARS: &[char] = &[
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}', '\u{200B}', '\u{00AD}', '\u{FEFF}', '\u{061C}', '\u{180E}', '\u{200E}', '\u{200F}',
];

/// Strip control (`Cc`) and spoof-capable format characters, then trim
/// ASCII/Unicode whitespace. The single cleaning step behind both the native
/// and import author policies — parity is structural, not conventional.
#[must_use]
pub fn clean_author_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && !SPOOF_CHARS.contains(c))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Live-submission author policy (native comments): clean, then require a
/// non-empty name unless a `github_username` fallback is present, then
/// reject over `max_author_len` (chars, matching `MAX_AUTHOR_LEN`).
pub fn native_author_name(
    raw: &str,
    github_username: Option<&str>,
    max_author_len: usize,
) -> Result<String, ValidationError> {
    let cleaned = clean_author_name(raw);
    if cleaned.is_empty() && github_username.is_none_or(|g| g.trim().is_empty()) {
        return Err(ValidationError::InvalidAuthor(
            "author_name must not be empty (or provide github_username)".to_string(),
        ));
    }
    if cleaned.chars().count() > max_author_len {
        return Err(ValidationError::TooLong {
            max: max_author_len,
            got: cleaned.chars().count(),
        });
    }
    Ok(cleaned)
}

/// Historical-data author policy (import restore): clean, reject empty,
/// CLAMP to `max_author_len` (chars). Clamp-not-reject is deliberate: an
/// export written under a larger limit must survive a restore under a
/// smaller one; live input is rejected instead (see [`native_author_name`]).
pub fn import_author_name(raw: &str, max_author_len: usize) -> Result<String, String> {
    let cleaned = clean_author_name(raw);
    if cleaned.is_empty() {
        return Err("author_name must not be empty".to_string());
    }
    if cleaned.chars().count() > max_author_len {
        Ok(cleaned.chars().take(max_author_len).collect())
    } else {
        Ok(cleaned)
    }
}

/// Validate a `github_username` before URL interpolation. Thin wrapper over
/// [`validate_github_username`] so both native paths (author resolve, avatar
/// resolve) spell the check identically.
pub fn check_github_username(raw: &str) -> Result<(), ValidationError> {
    validate_github_username(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidi_and_zero_width_stripped() {
        // RED (S2/F6): the old `strip_control_chars`-only path kept these.
        assert_eq!(clean_author_name("ab\u{202E}cd"), "abcd");
        assert_eq!(clean_author_name("a\u{200B}b"), "ab");
        assert_eq!(clean_author_name("\u{2066}hi\u{2069}"), "hi");
        assert_eq!(clean_author_name("a\u{202A}b\u{202C}c"), "abc");
        assert_eq!(clean_author_name("\u{FEFF}Ada"), "Ada");
        assert_eq!(clean_author_name("a\u{00AD}b"), "ab");
    }

    #[test]
    fn zwj_emoji_survives_cleaning() {
        // U+200D joins family emoji — must NOT be stripped.
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(clean_author_name(family), family);
    }

    #[test]
    fn controls_still_stripped_and_trimmed() {
        assert_eq!(clean_author_name("Bad\x00Guy"), "BadGuy");
        assert_eq!(clean_author_name("  Ada  "), "Ada");
    }

    #[test]
    fn native_rejects_empty_without_github() {
        assert!(native_author_name("", None, 100).is_err());
        assert!(native_author_name("   ", None, 100).is_err());
        assert!(native_author_name("", Some("alice"), 100).is_ok());
        // Bidi-only names clean to empty → same rejection.
        assert!(native_author_name("\u{202E}\u{200B}", None, 100).is_err());
    }

    #[test]
    fn native_rejects_over_max_len() {
        let long = "a".repeat(101);
        assert!(matches!(
            native_author_name(&long, None, 100),
            Err(ValidationError::TooLong { max: 100, got: 101 })
        ));
    }

    #[test]
    fn native_honors_configured_max_len() {
        let name = "a".repeat(51);
        assert!(native_author_name(&name, None, 50).is_err());
        assert!(native_author_name(&name, None, 100).is_ok());
    }

    #[test]
    fn import_clamps_to_configured_max_len() {
        // RED: the old path hardcoded 100 regardless of MAX_AUTHOR_LEN.
        let long = "b".repeat(60);
        assert_eq!(import_author_name(&long, 50).unwrap().chars().count(), 50);
        assert_eq!(import_author_name(&long, 100).unwrap().chars().count(), 60);
        assert!(import_author_name("", 100).is_err());
        assert_eq!(import_author_name("Ada", 100).unwrap(), "Ada");
    }

    #[test]
    fn github_shape_checked_before_interpolation() {
        assert!(check_github_username("alice").is_ok());
        assert!(check_github_username("a<b>\n").is_err());
        assert!(check_github_username("\u{202e}evil").is_err());
    }

    /// Parity table (S2): native and import accept/reject the SAME author
    /// set. Length is the one deliberate divergence (native rejects,
    /// import clamps) and is asserted separately above.
    #[test]
    fn native_import_parity_table() {
        let cases: &[(&str, bool)] = &[
            ("Ada", true),
            ("", false),
            ("   ", false),
            ("Bad\x00Guy", true),        // cleans to "BadGuy"
            ("ab\u{202E}cd", true),      // cleans to "abcd"
            ("a\u{200B}b", true),        // cleans to "ab"
            ("\u{202E}\u{200B}", false), // cleans to empty
            ("O'Brien", true),
            ("José 日本語", true),
        ];
        for (raw, accept) in cases {
            let native = native_author_name(raw, None, 100).is_ok();
            let import = import_author_name(raw, 100).is_ok();
            assert_eq!(native, *accept, "native policy on {raw:?}");
            assert_eq!(import, *accept, "import policy on {raw:?}");
            assert_eq!(native, import, "parity on {raw:?}");
        }
        // Deliberate divergence outside the table: an empty name WITH a
        // GitHub fallback is live-OK (the lookup fills it) but has no
        // historical meaning — exports never carry `github_username` — so
        // import still rejects the empty name.
        assert!(native_author_name("", Some("alice"), 100).is_ok());
        assert!(import_author_name("", 100).is_err());
    }
}
