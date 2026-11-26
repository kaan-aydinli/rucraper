//! Exact document deduplication on a persistent XXH3-128 hash index.
//!
//! The hash is computed over a canonical form of the document (lowercased,
//! punctuation removed, whitespace collapsed) so that pages differing only in
//! punctuation, casing or whitespace collapse to one entry. Insertion uses the
//! tree's atomic compare-and-swap, so two workers racing on the same document
//! cannot both claim it.

use std::collections::HashMap;
use std::sync::Mutex;

use dashmap::DashMap;
use xxhash_rust::xxh3::xxh3_128;

use crate::error::Result;

/// Canonical form used for hashing: alphanumeric words, lowercase, single spaces.
pub fn canonicalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            for lowered in ch.to_lowercase() {
                out.push(lowered);
            }
        } else if !out.is_empty() {
            pending_space = true;
        }
    }
    out
}

/// 128-bit digest of the canonical form of `text`.
pub fn exact_digest(text: &str) -> [u8; 16] {
    let canonical = canonicalize(text);
    xxh3_128(canonical.as_bytes()).to_be_bytes()
}

/// Persistent exact-duplicate index.
#[derive(Debug)]
pub struct ExactDedup {
    tree: sled::Tree,
}

impl ExactDedup {
    /// Wrap an existing tree.
    pub fn new(tree: sled::Tree) -> Self {
        Self { tree }
    }

    /// Atomically test-and-insert a digest. Returns `true` when it was already present.
    pub fn check_and_insert(&self, digest: &[u8; 16]) -> Result<bool> {
        let outcome =
            self.tree
                .compare_and_swap(digest.as_slice(), None::<&[u8]>, Some(&[][..]))?;
        Ok(outcome.is_err())
    }

    /// Number of indexed documents.
    pub fn len(&self) -> u64 {
        self.tree.len() as u64
    }

    /// True when the index holds no document.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }
}

/// Removes paragraphs that repeat more often than a threshold within a run.
///
/// This catches newspaper footers, licence notices and "share this article"
/// blurbs that are unique per URL but identical inside their own site. The
/// counter map is bounded: when it grows past `max_entries` it is cleared, so a
/// multi-billion-paragraph crawl still runs in constant memory.
#[derive(Debug)]
pub struct ParagraphDeduper {
    counts: DashMap<u64, u32>,
    threshold: u32,
    max_entries: usize,
    lock: Mutex<()>,
}

impl ParagraphDeduper {
    /// Create a deduper that strips paragraphs seen more than `threshold` times.
    pub fn new(threshold: u32, max_entries: usize) -> Self {
        Self {
            counts: DashMap::new(),
            threshold: threshold.max(1),
            max_entries: max_entries.max(128),
            lock: Mutex::new(()),
        }
    }

    /// Strip over-represented paragraphs; returns the cleaned document.
    pub fn dedup(&self, text: &str) -> String {
        if self.counts.len() >= self.max_entries {
            if let Ok(_guard) = self.lock.lock() {
                if self.counts.len() >= self.max_entries {
                    self.counts.clear();
                }
            }
        }

        let mut kept: Vec<&str> = Vec::new();
        let mut saw_heading = false;
        for paragraph in text.split("\n\n") {
            let trimmed = paragraph.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Never strip headings: they carry the document structure.
            if trimmed.starts_with('#') {
                saw_heading = true;
                kept.push(trimmed);
                continue;
            }
            if trimmed.chars().count() < 40 {
                kept.push(trimmed);
                continue;
            }
            let key = xxhash_rust::xxh3::xxh3_64(trimmed.as_bytes());
            let count = {
                let mut entry = self.counts.entry(key).or_insert(0);
                *entry += 1;
                *entry
            };
            if count > self.threshold {
                continue;
            }
            kept.push(trimmed);
        }

        if !saw_heading {
            return kept.join("\n\n");
        }
        kept.join("\n\n")
    }

    /// Number of tracked paragraphs (tests/diagnostics).
    pub fn tracked(&self) -> usize {
        self.counts.len()
    }
}

impl Default for ParagraphDeduper {
    fn default() -> Self {
        Self::new(3, 65_536)
    }
}

/// Convenience: count paragraph duplicates in a batch (used by tests and tooling).
pub fn duplicated_paragraphs(paragraphs: &[&str]) -> HashMap<u64, u32> {
    let mut counts = HashMap::new();
    for paragraph in paragraphs {
        let key = xxhash_rust::xxh3::xxh3_64(paragraph.as_bytes());
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalization_ignores_case_punctuation_and_spacing() {
        let a = canonicalize("Hello,   WORLD!  Rust-lang.");
        let b = canonicalize("hello world rust lang");
        assert_eq!(a, b);
        assert_eq!(exact_digest("A b c."), exact_digest("a  b c"));
    }

    #[test]
    fn exact_index_detects_repeats_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let digest = exact_digest("some document body");
        {
            let db = sled::Config::new().path(dir.path()).open().unwrap();
            let index = ExactDedup::new(db.open_tree("exact").unwrap());
            assert!(!index.check_and_insert(&digest).unwrap());
            assert!(index.check_and_insert(&digest).unwrap());
            assert_eq!(index.len(), 1);
        }
        let db = sled::Config::new().path(dir.path()).open().unwrap();
        let index = ExactDedup::new(db.open_tree("exact").unwrap());
        assert!(index.check_and_insert(&digest).unwrap());
    }

    #[test]
    fn paragraph_deduper_strips_repeated_blocks_but_keeps_headings() {
        let deduper = ParagraphDeduper::new(2, 1024);
        let template =
            "Subscribe to our newsletter for weekly updates about the platform and its roadmap.";
        for _ in 0..3 {
            let text = format!(
                "# Heading\n\n{template}\n\nUnique body paragraph number one about ownership."
            );
            let cleaned = deduper.dedup(&text);
            assert!(cleaned.contains("# Heading"));
        }
        let text =
            format!("# Heading\n\n{template}\n\nAnother unique body paragraph about borrowing.");
        let cleaned = deduper.dedup(&text);
        assert!(!cleaned.contains(template), "template kept: {cleaned}");
        assert!(cleaned.contains("# Heading"));
    }

    #[test]
    fn paragraph_counter_is_bounded() {
        let deduper = ParagraphDeduper::new(3, 128);
        for i in 0..300 {
            deduper.dedup(&format!(
                "Paragraph {i} with a reasonably long body to be tracked by the deduper."
            ));
        }
        assert!(deduper.tracked() <= 130, "tracked: {}", deduper.tracked());
    }
}
