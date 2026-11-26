//! Render a pruned DOM subtree as clean, structural Markdown.
//!
//! Headings keep their level, paragraphs are separated by blank lines, lists
//! become `-` items, `<pre>` becomes fenced code (with the language when the
//! markup exposes one), and tables degrade to readable ` | ` rows. Unicode is
//! NFKC-normalised, control characters are stripped and runs of blank lines are
//! collapsed, so the output is uniform across wildly different sites.

use ego_tree::NodeId;
use once_cell::sync::Lazy;
use regex::Regex;
use scraper::node::Node;
use scraper::Html;
use unicode_normalization::UnicodeNormalization;

use crate::config::ExtractConfig;

use super::sanitizer::is_pruned_element;
use super::{collapse_whitespace, element_of, find_descendants, tag_name, truncate_chars};

static CODE_CLASS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(?:language|lang|highlight|brush)[-:]([a-z0-9+#]+)")
        .expect("static regex: code language")
});

/// Render the subtree rooted at `root` into normalized Markdown.
pub fn render_markdown(document: &Html, root: NodeId, config: &ExtractConfig) -> String {
    let mut out = String::with_capacity(8 * 1024);
    render_children(document, root, &mut out, config, 0);
    cleanup(&out)
}

fn render_children(
    document: &Html,
    node_id: NodeId,
    out: &mut String,
    config: &ExtractConfig,
    depth: usize,
) {
    if depth > 512 {
        return;
    }
    let Some(node) = document.tree.get(node_id) else {
        return;
    };
    for child in node.children() {
        render_node(document, child.id(), out, config, depth + 1);
    }
}

fn render_node(
    document: &Html,
    node_id: NodeId,
    out: &mut String,
    config: &ExtractConfig,
    depth: usize,
) {
    if depth > 512 {
        return;
    }
    let Some(node) = document.tree.get(node_id) else {
        return;
    };
    match node.value() {
        Node::Text(text) => push_text(out, &text.text),
        Node::Element(element) => {
            if is_pruned_element(element) {
                return;
            }
            let name = element.name();
            match name {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = name.as_bytes()[1].saturating_sub(b'0') as usize;
                    let level = level.clamp(1, 6);
                    let text = inline_text(document, node_id);
                    if text.is_empty() {
                        return;
                    }
                    block_break(out);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                    out.push_str(&text);
                    block_break(out);
                }
                "p" => {
                    block_break(out);
                    render_children(document, node_id, out, config, depth + 1);
                    block_break(out);
                }
                "pre" => {
                    if config.keep_code_blocks {
                        render_code_block(document, node_id, out);
                    } else {
                        let text = inline_text(document, node_id);
                        if !text.is_empty() {
                            block_break(out);
                            out.push_str(&text);
                            block_break(out);
                        }
                    }
                }
                "code" => {
                    let text = raw_text(document, node_id);
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && !trimmed.contains('\n') {
                        out.push('`');
                        out.push_str(trimmed);
                        out.push('`');
                    } else if !trimmed.is_empty() {
                        out.push_str(trimmed);
                    }
                }
                "table" => {
                    if config.keep_tables {
                        render_table(document, node_id, out);
                    }
                }
                "ul" | "ol" | "dl" => {
                    block_break(out);
                    render_children(document, node_id, out, config, depth + 1);
                    block_break(out);
                }
                "li" => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("- ");
                    render_children(document, node_id, out, config, depth + 1);
                    out.push('\n');
                }
                "dt" => {
                    out.push_str("**");
                    render_children(document, node_id, out, config, depth + 1);
                    out.push_str("**\n");
                }
                "dd" => {
                    out.push_str(": ");
                    render_children(document, node_id, out, config, depth + 1);
                    out.push('\n');
                }
                "blockquote" => {
                    let mut inner = String::new();
                    render_children(document, node_id, &mut inner, config, depth + 1);
                    let quoted = quote_lines(&inner);
                    if !quoted.trim().is_empty() {
                        block_break(out);
                        out.push_str(&quoted);
                        block_break(out);
                    }
                }
                "br" => out.push('\n'),
                "hr" => {
                    block_break(out);
                    out.push_str("---");
                    block_break(out);
                }
                "figcaption" | "caption" => {
                    block_break(out);
                    out.push('*');
                    let text = inline_text(document, node_id);
                    out.push_str(&text);
                    out.push('*');
                    block_break(out);
                }
                "div" | "section" | "article" | "main" | "body" | "figure" | "details"
                | "summary" | "html" | "center" | "font" => {
                    block_break(out);
                    render_children(document, node_id, out, config, depth + 1);
                    block_break(out);
                }
                _ => render_children(document, node_id, out, config, depth + 1),
            }
        }
        _ => {}
    }
}

fn render_code_block(document: &Html, node_id: NodeId, out: &mut String) {
    let code = raw_text(document, node_id);
    let code = code.trim_matches('\n');
    if code.trim().is_empty() {
        return;
    }
    let language = code_language(document, node_id);
    let fence = if code.contains("```") { "~~~" } else { "```" };
    block_break(out);
    out.push_str(fence);
    if let Some(language) = language {
        out.push_str(&language);
    }
    out.push('\n');
    out.push_str(code);
    out.push('\n');
    out.push_str(fence);
    block_break(out);
}

fn code_language(document: &Html, node_id: NodeId) -> Option<String> {
    if let Some(element) = element_of(document, node_id) {
        if let Some(language) = language_from_element(element) {
            return Some(language);
        }
    }
    for id in find_descendants(document, node_id, &["code"]) {
        if tag_name(document, id) == Some("code") {
            if let Some(language) = element_of(document, id).and_then(language_from_element) {
                return Some(language);
            }
        }
    }
    None
}

fn language_from_element(element: &scraper::node::Element) -> Option<String> {
    if let Some(data_lang) = element.attr("data-lang") {
        let trimmed = data_lang.trim();
        if !trimmed.is_empty() {
            return Some(truncate_chars(&trimmed.to_ascii_lowercase(), 20));
        }
    }
    let class = element.attr("class")?;
    let captures = CODE_CLASS.captures(class)?;
    captures
        .get(1)
        .map(|m| truncate_chars(&m.as_str().to_ascii_lowercase(), 20))
}

fn render_table(document: &Html, node_id: NodeId, out: &mut String) {
    let rows = find_descendants(document, node_id, &["tr"]);
    if rows.is_empty() {
        block_break(out);
        out.push_str(&collapse_whitespace(&raw_text(document, node_id)));
        block_break(out);
        return;
    }

    let mut rendered = String::new();
    let mut cell_count = 0usize;
    let mut cell_chars = 0usize;
    for row in rows {
        let cells: Vec<String> = find_descendants(document, row, &["th", "td"])
            .into_iter()
            .map(|cell| collapse_whitespace(&raw_text(document, cell)))
            .filter(|cell| !cell.is_empty())
            .collect();
        if cells.is_empty() {
            continue;
        }
        cell_count += cells.len();
        cell_chars += cells.iter().map(|cell| cell.chars().count()).sum::<usize>();
        rendered.push_str(&cells.join(" | "));
        rendered.push('\n');
    }
    if rendered.is_empty() {
        return;
    }

    // Infoboxes and layout tables are grids of very short labels/values. Their
    // serialized form is noise, not prose, so drop them entirely. Real data
    // tables (or tables with any substance) are still rendered as pipe rows.
    if cell_count >= 6 && cell_chars / cell_count < 20 {
        tracing::trace!(cells = cell_count, "dropping low-density table");
        return;
    }

    block_break(out);
    out.push_str(&rendered);
    block_break(out);
}

fn quote_lines(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 8);
    for line in input.split('\n') {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            result.push_str(">\n");
        } else {
            result.push_str("> ");
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    result
}

/// Text of the subtree with whitespace preserved (used for code and cells).
fn raw_text(document: &Html, node_id: NodeId) -> String {
    let mut out = String::new();
    collect_raw_text(document, node_id, &mut out, 0);
    out
}

fn collect_raw_text(document: &Html, node_id: NodeId, out: &mut String, depth: usize) {
    if depth > 512 {
        return;
    }
    let Some(node) = document.tree.get(node_id) else {
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
                collect_raw_text(document, child.id(), out, depth + 1);
            }
            _ => {}
        }
    }
}

/// Whitespace-collapsed text of a subtree (used for headings and captions).
fn inline_text(document: &Html, node_id: NodeId) -> String {
    collapse_whitespace(&raw_text(document, node_id))
}

fn push_text(out: &mut String, raw: &str) {
    let text = collapse_whitespace(raw);
    if text.is_empty() {
        return;
    }
    let needs_space = out
        .chars()
        .last()
        .map(|c| !c.is_whitespace())
        .unwrap_or(false)
        && !text
            .chars()
            .next()
            .map(is_closing_punctuation)
            .unwrap_or(true);
    if needs_space {
        out.push(' ');
    }
    out.push_str(&text);
}

fn is_closing_punctuation(c: char) -> bool {
    matches!(
        c,
        '.' | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '%'
            | '…'
            | '\''
            | '"'
            | '’'
            | '”'
            | '»'
    )
}

fn block_break(out: &mut String) {
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
    if out.is_empty() {
        return;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
}

/// Final normalization pass: NFKC, control character removal, blank-line collapse.
pub fn cleanup(raw: &str) -> String {
    let normalized: String = raw.nfkc().collect();
    let mut lines: Vec<String> = Vec::new();
    for raw_line in normalized.split('\n') {
        let mut line = String::with_capacity(raw_line.len());
        for ch in raw_line.chars() {
            match ch {
                '\r' => {}
                '\t' => line.push(' '),
                '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{00ad}' => {}
                c if c.is_control() => {}
                c => line.push(c),
            }
        }
        let trimmed = line.trim_end().to_string();
        if trimmed.is_empty() {
            if lines.last().map(|last| last.is_empty()).unwrap_or(false) {
                continue;
            }
            lines.push(String::new());
        } else {
            lines.push(trimmed);
        }
    }
    while lines.first().map(|line| line.is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|line| line.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::scorer::select_content_root;

    fn render(html: &str) -> String {
        let document = Html::parse_document(html);
        let config = ExtractConfig::default();
        let root = select_content_root(&document, &config);
        render_markdown(&document, root, &config)
    }

    #[test]
    fn headings_paragraphs_lists_and_code_are_structural() {
        let markdown = render(
            "<html><body><article>
            <h1>Title</h1><h3>Sub</h3>
            <p>First paragraph with enough words to matter here.</p>
            <ul><li>one</li><li>two</li></ul>
            <pre class=\"language-python\"><code>print('hi')</code></pre>
            </article></body></html>",
        );
        assert!(markdown.contains("# Title"), "{markdown}");
        assert!(markdown.contains("### Sub"), "{markdown}");
        assert!(markdown.contains("First paragraph"), "{markdown}");
        assert!(markdown.contains("- one"), "{markdown}");
        assert!(markdown.contains("```python"), "{markdown}");
        assert!(markdown.contains("print('hi')"), "{markdown}");
    }

    #[test]
    fn tables_become_pipe_rows() {
        let markdown = render(
            "<html><body><article><p>Text before the table that is long enough.</p>
            <table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>
            </article></body></html>",
        );
        assert!(markdown.contains("A | B"), "{markdown}");
        assert!(markdown.contains("1 | 2"), "{markdown}");
    }

    #[test]
    fn cleanup_collapses_blank_lines_and_control_characters() {
        let dirty = "a\n\n\n\n\nb\u{0}\u{7}c\n\n";
        assert_eq!(cleanup(dirty), "a\n\nbc");
    }

    #[test]
    fn nested_quotes_and_line_breaks_render() {
        let markdown = render(
            "<html><body><article><p>Alpha beta gamma delta epsilon.</p>
             <blockquote><p>Quoted sentence.</p></blockquote>
             <p>Line one<br>Line two</p></article></body></html>",
        );
        assert!(markdown.contains("> Quoted sentence."), "{markdown}");
        assert!(markdown.contains("Line one\nLine two"), "{markdown}");
    }

    #[test]
    fn infobox_tables_are_dropped_but_data_tables_survive() {
        let infobox = r#"<html><body><article>
            <p>This paragraph carries enough prose for the density scorer to pick the article.</p>
            <table class="infobox">
              <tr><th>Capital</th><td>Ankara</td></tr>
              <tr><th>Language</th><td>Turkish</td></tr>
              <tr><th>Currency</th><td>Lira</td></tr>
            </table>
            <table class="data">
              <tr><th>Region</th><th>Population (2025 estimate)</th></tr>
              <tr><td>Marmara</td><td>Twenty four million residents reported</td></tr>
            </table>
            </article></body></html>"#;
        let markdown = render(infobox);
        assert!(!markdown.contains("Capital"), "infobox leaked: {markdown}");
        assert!(!markdown.contains("Ankara"), "infobox leaked: {markdown}");
        assert!(
            markdown.contains("Marmara | Twenty four million"),
            "{markdown}"
        );
    }

    #[test]
    fn code_fence_switches_when_backticks_appear() {
        let markdown = render(
            "<html><body><article><p>Enough prose to select this article block here.</p>
            <pre><code>let x = ```;</code></pre></article></body></html>",
        );
        assert!(markdown.contains("~~~"), "{markdown}");
    }
}
