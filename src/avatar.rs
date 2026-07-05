/// Avatar resolution utilities: best-effort extraction of author profile pictures
/// from website URLs. Supports h-card photos, favicons (multiple formats/sizes),
/// GitHub avatars, and sensible fallbacks.
///
/// Only compiled when the `webmentions` feature is enabled (uses `scraper`).
use scraper::{Html, Selector};
use std::sync::LazyLock;
use url::Url;

static ICON_LINK: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(
        "link[rel~='icon'], link[rel~='shortcut icon'], link[rel~='apple-touch-icon'], \
         link[rel~='apple-touch-icon-precomposed']",
    )
    .unwrap()
});
static OG_IMAGE: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("meta[property='og:image']").unwrap());

/// Parse an HTML page and extract the best favicon URL.
/// Handles multiple icon declarations, different formats, and sizes.
/// Returns `None` if no suitable icon is found.
///
/// Selection rules:
/// - Skip icons with `media="(prefers-color-scheme: dark)"` (prefer light/default)
/// - Prefer icons without a media query over those with one
/// - Prefer PNG over ICO over SVG (browser-like preference)
/// - Prefer larger sizes within the same format
/// - Fallback to `og:image` meta tag
pub fn best_favicon(html: &str, base_url: &Url) -> Option<String> {
    let doc = Html::parse_document(html);
    let mut candidates: Vec<FaviconCandidate> = Vec::new();

    for el in doc.select(&ICON_LINK) {
        let href = el.value().attr("href")?;
        let rel = el.value().attr("rel").unwrap_or("");
        let media = el.value().attr("media").unwrap_or("");
        let type_attr = el.value().attr("type").unwrap_or("");
        let sizes = el.value().attr("sizes").unwrap_or("");

        // Skip dark-theme variants
        if media.contains("prefers-color-scheme: dark") {
            continue;
        }

        // Resolve relative URL
        let abs = if let Ok(url) = base_url.join(href) {
            url
        } else {
            continue;
        };

        let is_dark = media.contains("prefers-color-scheme: light");
        let format = icon_format(type_attr, &abs, rel);
        let size = parse_icon_size(sizes);

        candidates.push(FaviconCandidate {
            url: abs,
            format,
            size,
            has_media: !media.is_empty(),
            is_dark,
        });
    }

    if candidates.is_empty() {
        // Fallback to og:image
        return og_image(&doc, base_url);
    }

    // Sort: prefer light/default, then PNG, then larger sizes
    candidates.sort_by(|a, b| {
        // Icons without media queries first
        a.has_media.cmp(&b.has_media).then_with(|| {
            // Prefer light over dark
            a.is_dark
                .cmp(&b.is_dark)
                .then_with(|| {
                    // Prefer PNG, then ICO, then SVG, then unknown
                    a.format.priority().cmp(&b.format.priority())
                })
                .then_with(|| {
                    // Larger size preferred
                    b.size.cmp(&a.size)
                })
        })
    });

    Some(candidates.into_iter().next()?.url.to_string())
}

/// Extract `og:image` meta tag content.
fn og_image(doc: &Html, base_url: &Url) -> Option<String> {
    let el = doc.select(&OG_IMAGE).next()?;
    let content = el.value().attr("content")?;
    base_url.join(content).ok().map(|u| u.to_string())
}

#[derive(Debug)]
struct FaviconCandidate {
    url: Url,
    format: IconFormat,
    /// Parsed size in pixels (0 = unknown/any)
    size: u32,
    /// True if this icon has a media query
    has_media: bool,
    /// True if this icon targets dark theme
    is_dark: bool,
}

/// Rough icon format classification for priority sorting.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IconFormat {
    Png,
    Ico,
    Svg,
    Other,
}

impl IconFormat {
    fn priority(&self) -> u8 {
        match self {
            Self::Png => 0,
            Self::Ico => 1,
            Self::Svg => 2,
            Self::Other => 3,
        }
    }
}

/// Determine icon format from type attribute, file extension, and rel attribute.
fn icon_format(type_attr: &str, url: &Url, rel: &str) -> IconFormat {
    if type_attr.contains("png") || url.path().ends_with(".png") {
        IconFormat::Png
    } else if type_attr.contains("svg") || url.path().ends_with(".svg") {
        IconFormat::Svg
    } else if type_attr.contains("x-icon")
        || url.path().ends_with(".ico")
        || rel.contains("shortcut")
        || rel.contains("apple-touch")
    {
        IconFormat::Ico
    } else {
        IconFormat::Other
    }
}

/// Parse the `sizes` attribute value (e.g. "32x32") into a number.
fn parse_icon_size(sizes: &str) -> u32 {
    if sizes == "any" {
        return 0;
    }
    sizes
        .split_whitespace()
        .filter_map(|s| s.split('x').next())
        .filter_map(|s| s.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

/// Default avatar URL when nothing else works.
pub const DEFAULT_AVATAR: &str =
    "https://upload.wikimedia.org/wikipedia/commons/a/ac/Default_pfp.jpg";

#[cfg(test)]
mod tests {
    use super::*;

    fn test_url() -> Url {
        Url::parse("https://example.com").unwrap()
    }

    #[test]
    fn no_icons_returns_none() {
        let html = "<html><body>no icons here</body></html>";
        assert_eq!(best_favicon(html, &test_url()), None);
    }

    #[test]
    fn single_png_icon() {
        let html =
            r#"<html><head><link rel="icon" href="/favicon.png" type="image/png"></head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(result, Some("https://example.com/favicon.png".to_string()));
    }

    #[test]
    fn single_ico_icon() {
        let html = r#"<html><head><link rel="icon" href="/favicon.ico"></head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(result, Some("https://example.com/favicon.ico".to_string()));
    }

    #[test]
    fn prefers_png_over_ico() {
        let html = r#"<html><head>
            <link rel="icon" href="/favicon.ico">
            <link rel="icon" href="/favicon.png" type="image/png">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/favicon.png".to_string()),
            "should prefer PNG over ICO"
        );
    }

    #[test]
    fn prefers_larger_size() {
        let html = r#"<html><head>
            <link rel="icon" href="/small.png" type="image/png" sizes="16x16">
            <link rel="icon" href="/large.png" type="image/png" sizes="64x64">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/large.png".to_string()),
            "should prefer larger size"
        );
    }

    #[test]
    fn skips_dark_theme_icon() {
        let html = r#"<html><head>
            <link rel="icon" href="/light.png" type="image/png">
            <link rel="icon" href="/dark.png" type="image/png" media="(prefers-color-scheme: dark)">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/light.png".to_string()),
            "should pick the light/default icon"
        );
    }

    #[test]
    fn prefers_no_media_query() {
        let html = r#"<html><head>
            <link rel="icon" href="/themed.png" type="image/png" media="(prefers-color-scheme: light)">
            <link rel="icon" href="/plain.png" type="image/png">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/plain.png".to_string()),
            "should prefer icon without media query"
        );
    }

    #[test]
    fn apple_touch_icon() {
        let html = r#"<html><head>
            <link rel="apple-touch-icon" href="/apple-touch-icon.png">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/apple-touch-icon.png".to_string())
        );
    }

    #[test]
    fn sizes_any_is_lowest_priority() {
        let html = r#"<html><head>
            <link rel="icon" href="/any.png" type="image/png" sizes="any">
            <link rel="icon" href="/fixed.png" type="image/png" sizes="32x32">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/fixed.png".to_string()),
            "should prefer fixed size over 'any'"
        );
    }

    #[test]
    fn svg_icon_is_last_resort() {
        let html = r#"<html><head>
            <link rel="icon" href="/icon.svg" type="image/svg+xml">
            <link rel="icon" href="/icon.png" type="image/png">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/icon.png".to_string()),
            "should prefer PNG over SVG"
        );
    }

    #[test]
    fn og_image_fallback() {
        let html = r#"<html><head>
            <meta property="og:image" content="https://example.com/og.jpg">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/og.jpg".to_string()),
            "should fallback to og:image"
        );
    }

    #[test]
    fn relative_url_resolved() {
        let html = r#"<html><head>
            <link rel="icon" href="subdir/favicon.png" type="image/png">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        assert_eq!(
            result,
            Some("https://example.com/subdir/favicon.png".to_string())
        );
    }

    #[test]
    fn multiple_icons_with_mixed_formats() {
        let html = r#"<html><head>
            <link rel="icon" href="/favicon.ico" sizes="32x32">
            <link rel="icon" href="/icon.svg" type="image/svg+xml">
            <link rel="icon" href="/icon-192.png" type="image/png" sizes="192x192">
            <link rel="icon" href="/icon-512.png" type="image/png" sizes="512x512">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        // Should prefer the largest PNG
        assert_eq!(
            result,
            Some("https://example.com/icon-512.png".to_string()),
            "should pick largest PNG"
        );
    }

    #[test]
    fn dark_and_light_icons() {
        let html = r#"<html><head>
            <link rel="icon" href="/dark.svg" type="image/svg+xml" media="(prefers-color-scheme: dark)">
            <link rel="icon" href="/light.svg" type="image/svg+xml" media="(prefers-color-scheme: light)">
            <link rel="icon" href="/fallback.png" type="image/png" sizes="32x32">
        </head></html>"#;
        let result = best_favicon(html, &test_url());
        // Should skip dark, prefer the one with media (light), but fallback.png has no media query and is PNG
        // Actually light.svg has a media query and is SVG (priority 2), fallback.png is PNG (priority 0)
        // The sorting: first b.has_media.cmp(&a.has_media) puts fallback.png first (no media)
        assert_eq!(
            result,
            Some("https://example.com/fallback.png".to_string()),
            "should prefer PNG without media query over SVG with light media"
        );
    }
}
