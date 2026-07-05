use scraper::{Html, Selector};
use std::sync::LazyLock;
use url::Url;

/// Structured data extracted from a webmention source page's microformats2 markup.
#[derive(Debug, Clone)]
pub struct ParsedMention {
    /// Sanitized HTML content from `.e-content` (or fallback).
    pub content: String,
    /// Author name (`.p-author .p-name` or fallback).
    pub author_name: String,
    /// Author URL (`.p-author .u-url`).
    pub author_url: Option<String>,
    /// Author photo URL (`.p-author .u-photo`).
    pub author_avatar: Option<String>,
}

static H_ENTRY: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".h-entry, .hentry").unwrap());
static H_CARD: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".h-card, .vcard").unwrap());
static P_NAME: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".p-name").unwrap());
static P_AUTHOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".p-author").unwrap());
static E_CONTENT: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".e-content").unwrap());
static U_URL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".u-url").unwrap());
static U_PHOTO: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".u-photo").unwrap());
static P_SUMMARY: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".p-summary").unwrap());
static A_HREF: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static LINK_HREF: LazyLock<Selector> = LazyLock::new(|| Selector::parse("link[href]").unwrap());

/// Extract a photo URL from the page's h-card markup.
/// Looks for `.h-card .u-photo` or a standalone `.u-photo` element.
/// Returns the absolute URL if found.
pub fn extract_photo(html: &str, base_url: &Url) -> Option<String> {
    let doc = Html::parse_document(html);

    // Try .h-card .u-photo first
    if let Some(hcard) = doc.select(&H_CARD).next() {
        if let Some(el) = hcard.select(&U_PHOTO).next() {
            if let Some(href) = el.value().attr("src").or(el.value().attr("href")) {
                if let Ok(abs) = base_url.join(href) {
                    return Some(abs.to_string());
                }
            }
        }
    }

    // Fallback: any .u-photo or img[class~='u-photo'] in the page
    for el in doc.select(&U_PHOTO) {
        if let Some(src) = el.value().attr("src").or(el.value().attr("href")) {
            if let Ok(abs) = base_url.join(src) {
                return Some(abs.to_string());
            }
        }
    }

    None
}

/// Check if the HTML from `source` contains a link to `target`.
pub fn has_backlink(html: &str, target: &str) -> bool {
    let doc = Html::parse_document(html);

    // Check <a href="..."> and <link href="..."> elements.
    for sel in [&*A_HREF, &*LINK_HREF] {
        for el in doc.select(sel) {
            if let Some(href) = el.value().attr("href")
                && urls_match(href, target)
            {
                return true;
            }
        }
    }
    false
}

fn urls_match(found: &str, target: &str) -> bool {
    // Simple match: found equals target, or resolves to target.
    if found == target {
        return true;
    }
    // If found is a relative URL, resolve against target's origin.
    if let Ok(target_url) = Url::parse(target)
        && let Ok(resolved) = target_url.join(found)
    {
        return resolved.as_str() == target;
    }
    false
}

/// Parse the first h-entry from the HTML and extract author/content.
/// Returns `None` if no h-entry is found.
pub fn parse_h_entry(html: &str) -> Option<ParsedMention> {
    let doc = Html::parse_document(html);
    let entry = doc.select(&H_ENTRY).next()?;

    // Content: .e-content HTML, then .p-summary, then .p-name, then fallback.
    let content = entry
        .select(&E_CONTENT)
        .next()
        .map(|el| el.inner_html())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            entry
                .select(&P_SUMMARY)
                .next()
                .map(|el| el.inner_html())
                .filter(|s| !s.trim().is_empty())
        })
        .or_else(|| {
            entry
                .select(&P_NAME)
                .next()
                .map(|el| el.text().collect::<String>())
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "Mentioned this page.".to_string());

    // Author: look for .p-author which may contain a nested .h-card.
    let (author_name, author_url, author_avatar) = parse_author(&entry).unwrap_or_else(|| {
        // Fallback: extract from the document body's domain.
        (String::new(), None, None)
    });

    Some(ParsedMention {
        content,
        author_name,
        author_url,
        author_avatar,
    })
}

/// Parse the `.p-author` element: either a nested `.h-card` or plain text.
fn parse_author(
    within: &scraper::ElementRef<'_>,
) -> Option<(String, Option<String>, Option<String>)> {
    let author_el = within.select(&P_AUTHOR).next()?;

    let text = author_el.text().collect::<String>();
    let trimmed = text.trim().to_string();

    // Check if the author element itself is an h-card (common mf2 pattern:
    // <a class="p-author h-card" href="...">Author Name</a>).
    // Then check for a nested .h-card child element.
    if let Some(card) = author_el.select(&H_CARD).next().or_else(|| {
        // The element itself might be the h-card (same element, both classes).
        // Match by re-checking the element's classes.
        if author_el
            .value()
            .has_class("h-card", scraper::CaseSensitivity::CaseSensitive)
        {
            Some(author_el)
        } else {
            None
        }
    }) {
        let name = card
            .select(&P_NAME)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| trimmed.to_string());

        let url = card
            .select(&U_URL)
            .next()
            .and_then(|el| el.value().attr("href"))
            .map(|s| s.to_string());
        let avatar = card
            .select(&U_PHOTO)
            .next()
            .and_then(|el| el.value().attr("src"))
            .map(|s| s.to_string());

        return Some((name, url, avatar));
    }

    // Plain text (or a bare <a>): check if author_el itself has u-url
    // or a child with href.
    let url = author_el
        .select(&U_URL)
        .next()
        .or_else(|| {
            if author_el
                .value()
                .has_class("u-url", scraper::CaseSensitivity::CaseSensitive)
            {
                Some(author_el)
            } else {
                None
            }
        })
        .and_then(|el| el.value().attr("href"))
        .map(|s| s.to_string());
    let url_from_a = author_el
        .select(&A_HREF)
        .next()
        .and_then(|el| el.value().attr("href"))
        .map(|s| s.to_string());

    Some((trimmed, url.or(url_from_a), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlink_found_in_anchor() {
        let html =
            r#"<html><body><a href="https://nithitsuki.com/blog/hello">my post</a></body></html>"#;
        assert!(has_backlink(html, "https://nithitsuki.com/blog/hello"));
    }

    #[test]
    fn backlink_not_found() {
        let html = r#"<html><body><a href="https://other.example">link</a></body></html>"#;
        assert!(!has_backlink(html, "https://nithitsuki.com/blog/hello"));
    }

    #[test]
    fn backlink_found_in_link_tag() {
        let html = r#"<html><head><link href="https://nithitsuki.com/blog/post" rel="mention" /></head></html>"#;
        assert!(has_backlink(html, "https://nithitsuki.com/blog/post"));
    }

    #[test]
    fn backlink_relative_url() {
        let html = r#"<html><body><a href="/blog/hello">link</a></body></html>"#;
        assert!(has_backlink(html, "https://nithitsuki.com/blog/hello"));
    }

    #[test]
    fn parse_h_entry_full() {
        let html = r#"<div class="h-entry">
            <div class="p-author h-card">
                <span class="p-name">Alice Green</span>
                <a class="u-url" href="https://alice.blog">alice.blog</a>
                <img class="u-photo" src="https://alice.blog/photo.jpg" />
            </div>
            <div class="e-content"><p>Great post, thanks!</p></div>
        </div>"#;

        let parsed = parse_h_entry(html).unwrap();
        assert_eq!(parsed.author_name, "Alice Green");
        assert_eq!(parsed.author_url, Some("https://alice.blog".to_string()));
        assert_eq!(
            parsed.author_avatar,
            Some("https://alice.blog/photo.jpg".to_string())
        );
        assert!(parsed.content.contains("Great post, thanks!"));
    }

    #[test]
    fn parse_h_entry_no_h_card_plain_text_author() {
        let html = r#"<div class="h-entry">
            <span class="p-author">Bob</span>
            <div class="e-content"><p>Nice!</p></div>
        </div>"#;

        let parsed = parse_h_entry(html).unwrap();
        assert_eq!(parsed.author_name, "Bob");
        assert!(parsed.author_url.is_none());
    }

    #[test]
    fn parse_h_entry_fallback_placeholder() {
        let html = r#"<div class="h-entry">
            <span class="p-author">Charlie</span>
        </div>"#;

        let parsed = parse_h_entry(html).unwrap();
        assert_eq!(parsed.author_name, "Charlie");
        assert_eq!(parsed.content, "Mentioned this page.");
    }

    #[test]
    fn parse_h_entry_missing_returns_none() {
        let html = r#"<html><body><p>no h-entry here</p></body></html>"#;
        assert!(parse_h_entry(html).is_none());
    }

    #[test]
    fn author_url_from_a_tag() {
        let html = r#"<div class="h-entry">
            <a class="p-author u-url" href="https://charlie.blog">Charlie</a>
            <div class="e-content">Hi</div>
        </div>"#;

        let parsed = parse_h_entry(html).unwrap();
        assert_eq!(parsed.author_name, "Charlie");
        assert_eq!(parsed.author_url, Some("https://charlie.blog".to_string()));
    }

    #[test]
    fn e_content_html_preserved() {
        let html = r#"<div class="h-entry">
            <span class="p-author">A</span>
            <div class="e-content"><p><strong>bold</strong> and <em>italic</em></p></div>
        </div>"#;

        let parsed = parse_h_entry(html).unwrap();
        assert!(parsed.content.contains("<strong>"));
        assert!(parsed.content.contains("<em>"));
    }
}
