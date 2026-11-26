//! Boilerplate detection at the DOM level.
//!
//! Two independent signals decide whether a subtree is noise:
//!
//! * the tag name (navigation, chrome, forms, embeds, scripts, ...),
//! * the `class`/`id` tokens (cookie banners, sidebars, comment threads, ...).
//!
//! Hidden elements (`hidden`, `aria-hidden`, inline `display:none`) are dropped
//! as well, because they never contribute readable prose.

use once_cell::sync::Lazy;
use regex::Regex;
use scraper::node::Element;

/// Tags whose content never belongs in a text corpus.
const PRUNED_TAGS: &[&str] = &[
    "script",
    "style",
    "noscript",
    "template",
    "svg",
    "canvas",
    "math",
    "nav",
    "footer",
    "header",
    "aside",
    "form",
    "button",
    "input",
    "select",
    "option",
    "textarea",
    "label",
    "iframe",
    "frame",
    "object",
    "embed",
    "applet",
    "video",
    "audio",
    "source",
    "track",
    "map",
    "area",
    "meta",
    "link",
    "base",
    "dialog",
    "ad",
    "iframe-fallback",
];

/// Class/ID substrings that mark a container as chrome rather than content.
static NOISE_ATTRIBUTE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(concat!(
        r"(?i)(cookie|consent|gdpr|ccpa|banner|popup|modal|overlay|sidebar|widget|",
        r"navbar|navigation|breadcrumb|pagination|pager|share|sharing|social|",
        r"comment|disqus|newsletter|subscribe|subscription|advert|adsense|adslot|promo|",
        r"sponsor|related|recommended|paywall|signup|sign-up|login|menu|toolbar|",
        r"skip-link|cookie-banner|site-header|site-footer)"
    ))
    .expect("static regex: noise attribute")
});

/// True when the element (and therefore its whole subtree) must be skipped.
pub fn is_pruned_element(element: &Element) -> bool {
    let tag = element.name();
    if PRUNED_TAGS.contains(&tag) {
        return true;
    }
    // Never prune semantic content containers just because of their attributes.
    if matches!(tag, "article" | "main" | "body" | "html") {
        return is_hidden(element);
    }
    has_noise_attribute(element) || is_hidden(element)
}

/// True when `class` or `id` contains a known boilerplate token.
pub fn has_noise_attribute(element: &Element) -> bool {
    for attribute in ["class", "id", "role", "aria-label"] {
        if let Some(value) = element.attr(attribute) {
            if NOISE_ATTRIBUTE.is_match(value) {
                return true;
            }
        }
    }
    false
}

/// True when the element is visually hidden or marked as presentational.
pub fn is_hidden(element: &Element) -> bool {
    if element.attr("hidden").is_some() {
        return true;
    }
    if element
        .attr("aria-hidden")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(style) = element.attr("style") {
        let compact: String = style.chars().filter(|c| !c.is_whitespace()).collect();
        let lowered = compact.to_ascii_lowercase();
        if lowered.contains("display:none") || lowered.contains("visibility:hidden") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Html;

    fn element_of(html: &str, selector: &str) -> Element {
        let document = Html::parse_fragment(html);
        let selector = scraper::Selector::parse(selector).unwrap();
        document.select(&selector).next().unwrap().value().clone()
    }

    #[test]
    fn prunes_chrome_tags() {
        for tag in [
            "nav", "footer", "header", "aside", "form", "script", "style", "svg",
        ] {
            let element = element_of(&format!("<{tag}>x</{tag}>"), tag);
            assert!(is_pruned_element(&element), "{tag} should be pruned");
        }
    }

    #[test]
    fn prunes_noisy_classes_and_ids() {
        for html in [
            "<div class=\"cookie-banner\">x</div>",
            "<div id=\"sidebar\">x</div>",
            "<div class=\"related-posts\">x</div>",
            "<section class=\"comments\" id=\"disqus_thread\">x</section>",
            "<div class=\"newsletter-signup\">x</div>",
            "<div class=\"advert banner\">x</div>",
        ] {
            let element = element_of(html, "div,section");
            assert!(is_pruned_element(&element), "{html} should be pruned");
        }
    }

    #[test]
    fn keeps_article_content() {
        for html in [
            "<div class=\"post-content\">x</div>",
            "<article class=\"article-body\">x</article>",
            "<main><p>x</p></main>",
        ] {
            let element = element_of(html, "div,article,main");
            assert!(!is_pruned_element(&element), "{html} should be kept");
        }
    }

    #[test]
    fn prunes_hidden_elements() {
        let element = element_of("<div style=\"display: none\">x</div>", "div");
        assert!(is_pruned_element(&element));
        let element = element_of("<div aria-hidden=\"true\">x</div>", "div");
        assert!(is_pruned_element(&element));
        let element = element_of("<div hidden>x</div>", "div");
        assert!(is_pruned_element(&element));
    }
}
