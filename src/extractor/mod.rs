//! Two-stage boilerplate removal: DOM pruning + Main-Content detection, then
//! structural Markdown rendering.
//!
//! Traversal is done on the `ego_tree` node ids rather than on `ElementRef`
//! selectors: the pruning rules need access to text nodes (which selectors skip)
//! and the renderer needs exact document order, so a single recursive walk with
//! an explicit pruning predicate is both simpler and faster than repeated CSS
//! selection.

pub mod formatter;
pub mod sanitizer;
pub mod scorer;

use std::collections::HashSet;

use ego_tree::NodeId;
use scraper::node::{Element, Node};
use scraper::Html;
use unicode_normalization::UnicodeNormalization;
use url::Url;

use crate::config::ExtractConfig;
use crate::frontier::normalizer;

use self::sanitizer::is_pruned_element;

/// Maximum DOM depth we are willing to descend (guards pathological documents).
const MAX_DEPTH: usize = 512;

/// Result of cleaning one HTML document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtractedDocument {
    /// Document title (from `<title>` or the first `<h1>`).
    pub title: String,
    /// Structural Markdown text.
    pub text: String,
    /// Absolute, normalized links discovered on the page.
    pub links: Vec<String>,
    /// Whitespace separated token count of `text`.
    pub word_count: usize,
}

/// Extract the main content of `html` with `base` as the URL resolution origin.
pub fn extract(html: &str, base: &Url, config: &ExtractConfig) -> ExtractedDocument {
    let document = Html::parse_document(html);
    let title = extract_title(&document, config);
    let links = collect_links(&document, base, config);
    let root = scorer::select_content_root(&document, config);
    let text = formatter::render_markdown(&document, root, config);
    let word_count = text.split_whitespace().count();
    ExtractedDocument {
        title,
        text,
        links,
        word_count,
    }
}

/// Borrow the element stored at `id`, if the node is an element and not pruned.
pub fn element_of(document: &Html, id: NodeId) -> Option<&Element> {
    match document.tree.get(id)?.value() {
        Node::Element(element) => Some(element),
        _ => None,
    }
}

/// Tag name of the node at `id`.
pub fn tag_name(document: &Html, id: NodeId) -> Option<&str> {
    element_of(document, id).map(Element::name)
}

/// Collect the ids of every descendant (document order) whose tag is in `tags`.
pub fn find_descendants(document: &Html, root: NodeId, tags: &[&str]) -> Vec<NodeId> {
    let mut found = Vec::new();
    collect_tagged(document, root, tags, &mut found, 0);
    found
}

fn collect_tagged(
    document: &Html,
    root: NodeId,
    tags: &[&str],
    found: &mut Vec<NodeId>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(node) = document.tree.get(root) else {
        return;
    };
    for child in node.children() {
        if let Node::Element(element) = child.value() {
            if tags.contains(&element.name()) {
                found.push(child.id());
            }
        }
        collect_tagged(document, child.id(), tags, found, depth + 1);
    }
}

/// First element with one of `tags` in the whole document (document order).
pub fn find_first(document: &Html, tags: &[&str]) -> Option<NodeId> {
    find_descendants(document, document.tree.root().id(), tags)
        .into_iter()
        .next()
}

/// Concatenated text of a subtree, skipping pruned elements.
///
/// When `collapse` is true every whitespace run becomes a single space and the
/// result is trimmed; otherwise the original spacing (including `<br>` newlines)
/// is preserved, which is what code blocks and table cells need.
pub fn collect_text(document: &Html, root: NodeId, collapse: bool) -> String {
    let mut out = String::new();
    collect_text_into(document, root, &mut out, 0);
    if collapse {
        collapse_whitespace(&out)
    } else {
        out
    }
}

fn collect_text_into(document: &Html, root: NodeId, out: &mut String, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(node) = document.tree.get(root) else {
        return;
    };
    for child in node.children() {
        match child.value() {
            Node::Text(text) => out.push_str(&text.text),
            Node::Element(element) => {
                if is_pruned_element(element) {
                    continue;
                }
                if element.name() == "br" {
                    out.push('\n');
                    continue;
                }
                collect_text_into(document, child.id(), out, depth + 1);
            }
            _ => {}
        }
    }
}

fn extract_title(document: &Html, config: &ExtractConfig) -> String {
    let raw = find_first(document, &["title"])
        .map(|id| collect_text(document, id, true))
        .filter(|text| !text.trim().is_empty())
        .or_else(|| find_first(document, &["h1"]).map(|id| collect_text(document, id, true)))
        .unwrap_or_default();
    let normalized: String = collapse_whitespace(&raw).nfkc().collect();
    truncate_chars(&normalized, config.max_title_chars)
}

fn collect_links(document: &Html, base: &Url, config: &ExtractConfig) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut links = Vec::new();
    for id in find_descendants(document, document.tree.root().id(), &["a"]) {
        if links.len() >= config.max_links_per_page {
            break;
        }
        let Some(element) = element_of(document, id) else {
            continue;
        };
        let Some(href) = element.attr("href") else {
            continue;
        };
        let Some(url) = normalizer::normalize(base, href) else {
            continue;
        };
        let normalized = url.as_str().to_string();
        if seen.insert(normalized.clone()) {
            links.push(normalized);
        }
    }
    links
}

/// Collapse every whitespace run into a single space and trim.
pub fn collapse_whitespace(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(ch);
    }
    output
}

/// Truncate to `max` characters on a `char` boundary.
pub fn truncate_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut result = String::with_capacity(input.len().min(max));
    for (count, ch) in input.chars().enumerate() {
        if count >= max {
            break;
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOISY_HTML: &str = r#"
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>  Rust İçin Sistem Programlama 101 | Example Blog </title>
  <style>body { color: red; }</style>
  <script>window.alert("noise");</script>
</head>
<body>
  <div id="cookie-consent" class="cookie-banner">We use cookies. <button>Accept</button></div>
  <header class="site-header"><nav><a href="/">Home</a><a href="/blog">Blog</a></nav></header>
  <div class="layout">
    <aside class="sidebar widget"><h3>Popular</h3><ul><li><a href="/p1">Post one</a></li></ul></aside>
    <main>
      <article>
        <h1>Rust İçin Sistem Programlama 101</h1>
        <p>Rust is a systems programming language focused on safety, speed, and concurrency.</p>
        <p>Its ownership model eliminates whole classes of memory errors without a garbage collector, which makes it a strong fit for long running services.</p>
        <h2>Borrowing</h2>
        <p>The borrow checker validates references at compile time, so data races and use after free bugs are rejected before the program ever runs on real hardware.</p>
        <pre class="language-rust"><code>fn main() {
    let values = vec![1, 2, 3];
    println!("{values:?}");
}</code></pre>
        <table>
          <tr><th>Feature</th><th>Cost</th></tr>
          <tr><td>Ownership</td><td>Compile time</td></tr>
        </table>
        <ul><li>Zero cost abstractions</li><li>Fearless concurrency</li></ul>
      </article>
      <section class="comments" id="disqus_thread">
        <h3>Comments</h3>
        <p>Great article! I learned a lot from this and I will share it with my team.</p>
      </section>
      <div class="related-posts"><a href="/other">Related reading</a></div>
    </main>
    <div class="advert banner promo"><p>Buy our course now and save fifty percent today only.</p></div>
  </div>
  <footer class="site-footer"><p>Copyright 2026 Example</p></footer>
</body>
</html>
"#;

    #[test]
    fn removes_boilerplate_and_keeps_article_structure() {
        let base = Url::parse("https://example.com/blog/rust-101").unwrap();
        let config = ExtractConfig::default();
        let doc = extract(NOISY_HTML, &base, &config);

        assert!(doc.title.starts_with("Rust İçin Sistem Programlama 101"));
        assert!(
            doc.text.starts_with("# Rust İçin Sistem Programlama 101"),
            "text: {}",
            doc.text
        );
        assert!(doc.text.contains("## Borrowing"));
        assert!(doc.text.contains("borrow checker") || doc.text.contains("compiler"));
        assert!(
            doc.text.contains("```rust"),
            "code fence missing: {}",
            doc.text
        );
        assert!(doc.text.contains("fn main()"));
        assert!(doc.text.contains("- Zero cost abstractions"));
        assert!(doc.text.contains("Feature | Cost"));

        for noise in [
            "We use cookies",
            "Popular",
            "Home",
            "Comments",
            "Great article",
            "Buy our course",
            "Copyright 2026",
            "window.alert",
            "color: red",
        ] {
            assert!(
                !doc.text.contains(noise),
                "noise leaked ({noise}): {}",
                doc.text
            );
        }
    }

    #[test]
    fn discovers_and_normalizes_links() {
        let base = Url::parse("https://example.com/blog/post").unwrap();
        let doc = extract(NOISY_HTML, &base, &ExtractConfig::default());
        assert!(doc.links.contains(&"https://example.com/".to_string()));
        assert!(doc.links.contains(&"https://example.com/blog".to_string()));
        assert!(doc.links.iter().all(|link| !link.contains('#')));
    }

    #[test]
    fn whitespace_and_titles_are_bounded() {
        assert_eq!(collapse_whitespace("  a \n\t b   c  "), "a b c");
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("abc", 0), "");
        let config = ExtractConfig {
            max_title_chars: 5,
            ..ExtractConfig::default()
        };
        let base = Url::parse("https://example.com/").unwrap();
        let doc = extract(
            "<html><head><title>abcdefgh</title></head><body><p>x</p></body></html>",
            &base,
            &config,
        );
        assert_eq!(doc.title, "abcde");
    }

    #[test]
    fn empty_and_broken_html_is_handled() {
        let base = Url::parse("https://example.com/").unwrap();
        let doc = extract("", &base, &ExtractConfig::default());
        assert!(doc.text.is_empty());
        assert_eq!(doc.word_count, 0);
        let doc = extract("<div><p>unclosed <b>tags", &base, &ExtractConfig::default());
        assert!(doc.text.contains("unclosed"));
    }
}
