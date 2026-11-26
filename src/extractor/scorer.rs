//! Main-content selection by text/link density.
//!
//! For every block container (`article`, `main`, `section`, `div`, `body`) the
//! extractor measures how much readable text it holds, how much of that text
//! sits inside links, and how many tags are needed to carry it. Blocks with a
//! link-text ratio above the threshold are treated as link farms. Among the
//! remaining blocks, the tightest container holding at least 65% of the best
//! text mass wins, which selects `<article>`/`<main>` instead of their wrapper
//! `<div>`s (the wrapper needs more tags to carry the same text).

use ego_tree::NodeId;
use scraper::node::Node;
use scraper::Html;

use crate::config::ExtractConfig;

use super::sanitizer::is_pruned_element;
use super::tag_name;

const CANDIDATE_TAGS: &[&str] = &["article", "main", "section", "div", "body"];

#[derive(Debug, Default, Clone, Copy)]
struct Metrics {
    text: usize,
    link_text: usize,
    paragraphs: usize,
    tags: usize,
}

#[derive(Debug)]
struct Candidate {
    id: NodeId,
    metrics: Metrics,
    tag_rank: u8,
}

/// Pick the DOM node that holds the main article body.
pub fn select_content_root(document: &Html, config: &ExtractConfig) -> NodeId {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut largest = 0usize;

    for id in super::find_descendants(document, document.tree.root().id(), CANDIDATE_TAGS) {
        let Some(tag) = tag_name(document, id) else {
            continue;
        };
        let mut metrics = Metrics::default();
        measure_subtree(document, id, false, &mut metrics);
        if metrics.text < config.min_block_chars {
            continue;
        }
        let link_ratio = if metrics.text == 0 {
            0.0
        } else {
            metrics.link_text as f64 / metrics.text as f64
        };
        if link_ratio > config.max_link_ratio {
            continue;
        }
        largest = largest.max(metrics.text);
        candidates.push(Candidate {
            id,
            metrics,
            tag_rank: tag_rank(tag),
        });
    }

    if candidates.is_empty() || largest == 0 {
        return fallback_root(document);
    }

    let floor = (largest as f64 * 0.65) as usize;
    let mut best: Option<&Candidate> = None;
    for candidate in &candidates {
        if candidate.metrics.text < floor {
            continue;
        }
        let better = match best {
            None => true,
            Some(current) => {
                candidate.metrics.tags < current.metrics.tags
                    || (candidate.metrics.tags == current.metrics.tags
                        && candidate.tag_rank < current.tag_rank)
                    || (candidate.metrics.tags == current.metrics.tags
                        && candidate.tag_rank == current.tag_rank
                        && candidate.metrics.paragraphs > current.metrics.paragraphs)
            }
        };
        if better {
            best = Some(candidate);
        }
    }

    best.map(|candidate| candidate.id)
        .unwrap_or_else(|| fallback_root(document))
}

/// Walk a subtree collecting text/link/paragraph/tag metrics, skipping pruned nodes.
fn measure_subtree(document: &Html, node_id: NodeId, in_link: bool, metrics: &mut Metrics) {
    let Some(node) = document.tree.get(node_id) else {
        return;
    };
    for child in node.children() {
        match child.value() {
            Node::Text(text) => {
                let length = text.text.chars().filter(|c| !c.is_whitespace()).count();
                metrics.text += length;
                if in_link {
                    metrics.link_text += length;
                }
            }
            Node::Element(element) => {
                if is_pruned_element(element) {
                    continue;
                }
                metrics.tags += 1;
                let name = element.name();
                let enters_link = in_link || name == "a";
                if name == "p" {
                    metrics.paragraphs += 1;
                }
                measure_subtree(document, child.id(), enters_link, metrics);
            }
            _ => {}
        }
    }
}

fn tag_rank(name: &str) -> u8 {
    match name {
        "article" => 0,
        "main" => 1,
        "section" => 2,
        "div" => 3,
        _ => 4,
    }
}

fn fallback_root(document: &Html) -> NodeId {
    super::find_first(document, &["body"]).unwrap_or_else(|| document.tree.root().id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Html;

    fn selected_tag(html: &str) -> String {
        let document = Html::parse_document(html);
        let id = select_content_root(&document, &ExtractConfig::default());
        tag_name(&document, id).unwrap_or("text").to_string()
    }

    #[test]
    fn picks_article_over_wrapper() {
        let html = r#"
        <html><body>
          <div class="wrapper">
            <nav><a href="/a">A</a><a href="/b">B</a></nav>
            <article>
              <p>The borrow checker rejects data races at compile time, which is a very useful property for systems software that must stay correct under heavy concurrency.</p>
              <p>Ownership keeps memory management deterministic without a garbage collector, and lifetimes describe how references relate to the values they point at.</p>
            </article>
          </div>
        </body></html>"#;
        assert_eq!(selected_tag(html), "article");
    }

    #[test]
    fn rejects_link_farm_blocks() {
        let mut links = String::new();
        for i in 0..40 {
            links.push_str(&format!(
                "<div><a href=\"/page-{i}\">Link number {i} with label</a></div>"
            ));
        }
        let html = format!(
            r#"<html><body><div id="links">{links}</div>
            <main><p>Real content lives here and it is long enough to be considered a block of prose by the density scorer.</p>
            <p>A second paragraph makes the decision unambiguous for the tightest container heuristic.</p></main></body></html>"#
        );
        assert_eq!(selected_tag(&html), "main");
    }

    #[test]
    fn falls_back_to_body_when_nothing_matches() {
        let html = "<html><body><span>x</span></body></html>";
        assert_eq!(selected_tag(html), "body");
    }
}
