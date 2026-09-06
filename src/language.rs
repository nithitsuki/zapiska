//! Optional language filtering for native comments.
//!
//! Controlled by three env vars (all off by default):
//! - `COMMENT_LANG_ALLOWED` — whitelist of ISO 639-1 codes (e.g. `en,de,ja`).
//! - `COMMENT_LANG_BLOCKED` — blacklist of ISO 639-1 codes.
//! - `COMMENT_LANG_ALLOW_EMOJI` — `always` (default) | `never` | `if_unknown`.
//!
//! Semantics:
//! - Neither set → filtering is off; every comment passes.
//! - Whitelist set → only those languages pass (whitelist wins if both set).
//! - Blacklist set (no whitelist) → those languages are rejected.
//! - "Unknown" content (no confident detection): emoji-heavy comments follow
//!   `COMMENT_LANG_ALLOW_EMOJI`; other undetectable text (gibberish, symbol
//!   dumps) passes under `always`/`never` and is rejected under `if_unknown`.
//!
//! Detection runs on the sanitized, tag-stripped content via `whatlang`
//! (ISO 639-3 internally; the config uses ISO 639-1, mapped here). The engine
//! is isolated in this module so it can be swapped without touching handlers.

use std::collections::HashSet;

use whatlang::Lang;

use crate::config::{Config, EmojiPolicy};

/// Confidence below which a detection is treated as "unknown".
const MIN_CONFIDENCE: f64 = 0.5;
/// A comment is "emoji-heavy" when at least this fraction of its
/// non-whitespace characters are emoji.
const EMOJI_HEAVY_RATIO: f64 = 0.5;

/// Compiled language gate, built once at startup from `Config`.
#[derive(Debug, Clone, Default)]
pub struct LanguageGate {
    whitelist: Option<HashSet<Lang>>,
    blacklist: HashSet<Lang>,
    emoji_policy: EmojiPolicy,
}

impl LanguageGate {
    pub fn new(config: &Config) -> Self {
        let whitelist = if config.comment_lang_allowed.is_empty() {
            None
        } else {
            Some(
                config
                    .comment_lang_allowed
                    .iter()
                    .map(String::as_str)
                    .filter_map(lang_from_iso_639_1)
                    .collect(),
            )
        };
        let blacklist: HashSet<Lang> = config
            .comment_lang_blocked
            .iter()
            .map(String::as_str)
            .filter_map(lang_from_iso_639_1)
            .collect();
        let emoji_policy = config.comment_lang_allow_emoji;
        Self {
            whitelist,
            blacklist,
            emoji_policy,
        }
    }

    /// Filtering is off when neither set is configured.
    pub fn is_enabled(&self) -> bool {
        self.whitelist.is_some() || !self.blacklist.is_empty()
    }

    /// Check sanitized comment content. Returns an error message (used in
    /// the 400 response) when the comment must be rejected.
    pub fn check(&self, content: &str) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        let plain = strip_html(content);
        let detected = whatlang::detect(&plain);

        let lang = match detected {
            Some(info) if info.confidence() >= MIN_CONFIDENCE => Some(info.lang()),
            _ => None,
        };

        match lang {
            Some(lang) => {
                if let Some(whitelist) = &self.whitelist {
                    if !whitelist.contains(&lang) {
                        return Err(format!(
                            "language '{}' is not in the allowed list",
                            lang.code()
                        ));
                    }
                } else if self.blacklist.contains(&lang) {
                    return Err(format!("language '{}' is blocked", lang.code()));
                }
                Ok(())
            }
            None => {
                // Unknown: route by emoji-heaviness and policy.
                let heavy = emoji_heavy(&plain);
                match self.emoji_policy {
                    EmojiPolicy::Always => Ok(()),
                    EmojiPolicy::Never if heavy => {
                        Err("emoji-only comments are not allowed".to_string())
                    }
                    EmojiPolicy::Never => Ok(()),
                    EmojiPolicy::IfUnknown if heavy => Ok(()),
                    EmojiPolicy::IfUnknown => {
                        Err("comment language could not be determined".to_string())
                    }
                }
            }
        }
    }
}

/// Map an ISO 639-1 code to whatlang's `Lang` (ISO 639-3 internally).
/// Covers every language whatlang supports that has a 639-1 code.
pub fn lang_from_iso_639_1(code: &str) -> Option<Lang> {
    use Lang::*;
    Some(match code {
        "af" => Afr,
        "am" => Amh,
        "ar" => Ara,
        "az" => Aze,
        "be" => Bel,
        "bg" => Bul,
        "bn" => Ben,
        "ca" => Cat,
        "cs" => Ces,
        "cy" => Cym,
        "da" => Dan,
        "de" => Deu,
        "el" => Ell,
        "en" => Eng,
        "eo" => Epo,
        "es" => Spa,
        "et" => Est,
        "fa" => Pes,
        "fi" => Fin,
        "fr" => Fra,
        "gu" => Guj,
        "he" => Heb,
        "hi" => Hin,
        "hr" => Hrv,
        "hu" => Hun,
        "hy" => Hye,
        "id" => Ind,
        "it" => Ita,
        "ja" => Jpn,
        "jv" => Jav,
        "ka" => Kat,
        "km" => Khm,
        "kn" => Kan,
        "ko" => Kor,
        "lt" => Lit,
        "lv" => Lav,
        "mk" => Mkd,
        "ml" => Mal,
        "mr" => Mar,
        "my" => Mya,
        "nb" => Nob,
        "ne" => Nep,
        "nl" => Nld,
        "pa" => Pan,
        "pl" => Pol,
        "pt" => Por,
        "ro" => Ron,
        "ru" => Rus,
        "si" => Sin,
        "sk" => Slk,
        "sl" => Slv,
        "sn" => Sna,
        "sr" => Srp,
        "sv" => Swe,
        "ta" => Tam,
        "te" => Tel,
        "th" => Tha,
        "tl" => Tgl,
        "tr" => Tur,
        "uk" => Ukr,
        "ur" => Urd,
        "uz" => Uzb,
        "vi" => Vie,
        "yi" => Yid,
        "zh" => Cmn,
        "zu" => Zul,
        _ => return None,
    })
}

/// Strip HTML tags (plain-text view for the detector).
fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => {
                if !in_tag && !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
                in_tag = true;
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// True when at least half of the non-whitespace characters are emoji.
fn emoji_heavy(text: &str) -> bool {
    let mut total = 0usize;
    let mut emoji = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        if is_emoji_char(c) {
            emoji += 1;
        }
    }
    total > 0 && emoji as f64 / total as f64 >= EMOJI_HEAVY_RATIO
}

/// Heuristic emoji classification (no external crate). Covers the common
/// pictograph, dingbat, and modifier ranges; not exhaustive, good enough for
/// a spam gate.
fn is_emoji_char(c: char) -> bool {
    let cp = c as u32;
    matches!(cp,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139
        | 0x2194..=0x2199 | 0x21A9..=0x21AA
        | 0x231A..=0x231B | 0x2328 | 0x23CF | 0x23E9..=0x23F3 | 0x23F8..=0x23FA | 0x24C2
        | 0x25AA..=0x25AB | 0x25B6 | 0x25C0 | 0x25FB..=0x25FE
        | 0x2600..=0x27BF
        | 0x2934..=0x2935 | 0x2B05..=0x2B07 | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x1F000..=0x1FAFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(allowed: &str, blocked: &str, emoji: &str) -> LanguageGate {
        LanguageGate {
            whitelist: if allowed.is_empty() {
                None
            } else {
                Some(allowed.split(',').filter_map(lang_from_iso_639_1).collect())
            },
            blacklist: blocked
                .split(',')
                .filter(|c| !c.is_empty())
                .filter_map(lang_from_iso_639_1)
                .collect(),
            emoji_policy: emoji.parse().expect("test emoji policy valid"),
        }
    }

    #[test]
    fn iso_639_1_mapping() {
        assert_eq!(lang_from_iso_639_1("en"), Some(Lang::Eng));
        assert_eq!(lang_from_iso_639_1("de"), Some(Lang::Deu));
        assert_eq!(lang_from_iso_639_1("ja"), Some(Lang::Jpn));
        assert_eq!(lang_from_iso_639_1("zh"), Some(Lang::Cmn));
        assert_eq!(lang_from_iso_639_1("xx"), None);
        assert_eq!(lang_from_iso_639_1(""), None);
    }

    #[test]
    fn disabled_when_nothing_configured() {
        let g = gate("", "", "always");
        assert!(!g.is_enabled());
        assert!(g.check("こんにちは世界").is_ok());
        assert!(g.check("<script>alert(1)</script>").is_ok());
    }

    #[test]
    fn whitelist_accepts_only_listed_languages() {
        let g = gate("en,de", "", "always");
        assert!(
            g.check("This is a perfectly normal English sentence.")
                .is_ok()
        );
        assert!(g.check("Das ist ein ganz normaler deutscher Satz.").is_ok());
        assert!(g.check("これは日本語の文章です。").is_err());
        assert!(
            g.check("Nous avons décidé de visiter le musée du Louvre avant de déjeuner.")
                .is_err()
        );
    }

    #[test]
    fn blacklist_rejects_listed_languages() {
        let g = gate("", "ja", "always");
        assert!(g.check("This is English.").is_ok());
        assert!(g.check("これは日本語の文章です。").is_err());
    }

    #[test]
    fn whitelist_wins_when_both_set() {
        let g = gate("en", "en", "always");
        assert!(
            g.check("English sentence here.").is_ok(),
            "whitelisted wins"
        );
        assert!(g.check("日本語の文章。").is_err());
    }

    #[test]
    fn emoji_policy_matrix() {
        let emoji_only = "👍👍👍";
        let gibberish = "qzx qzx qzx qzx qzx qzx";

        // always: everything passes (filtering only catches detected langs)
        let g = gate("en", "", "always");
        assert!(g.check(emoji_only).is_ok());
        assert!(g.check(gibberish).is_ok());

        // never: emoji-heavy rejected, other unknown passes
        let g = gate("en", "", "never");
        assert!(g.check(emoji_only).is_err());
        assert!(g.check(gibberish).is_ok());
        assert!(g.check("Nice comment 👍").is_ok(), "mixed text not heavy");

        // if_unknown: emoji accepted, gibberish rejected
        let g = gate("en", "", "if_unknown");
        assert!(g.check(emoji_only).is_ok());
        assert!(g.check(gibberish).is_err());
    }

    #[test]
    fn emoji_heaviness() {
        assert!(emoji_heavy("👍👍👍"));
        assert!(emoji_heavy("🎉🎉🎉🎉"));
        assert!(!emoji_heavy("Nice comment 👍"));
        assert!(!emoji_heavy(""));
        assert!(!emoji_heavy("plain text"));
    }

    #[test]
    fn html_is_stripped_before_detection() {
        let g = gate("en", "", "always");
        assert!(g.check("<p>This is an English comment.</p>").is_ok());
        assert!(g.check("<p>これは日本語です。</p>").is_err());
    }

    #[test]
    fn short_text_without_confidence_is_unknown() {
        // "Hi" is too short for a confident detection; under a whitelist with
        // if_unknown it is rejected rather than guessed.
        let g = gate("en", "", "if_unknown");
        assert!(g.check("Hi").is_err(), "low confidence treated as unknown");
        let g = gate("en", "", "always");
        assert!(g.check("Hi").is_ok());
    }
}
