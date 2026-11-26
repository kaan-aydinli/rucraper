//! Two-stage deduplication: exact XXH3-128 plus SimHash/LSH near duplicates.

pub mod exact;
pub mod simhash;

use crate::config::DedupConfig;
use crate::error::Result;
use crate::filter::RejectReason;

use self::exact::{exact_digest, ExactDedup, ParagraphDeduper};
use self::simhash::{simhash64, SimHashIndex};

/// Text after paragraph cleanup plus its duplicate verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessedText {
    /// Text with template paragraphs stripped (when enabled).
    pub text: String,
    /// 64-bit SimHash fingerprint.
    pub simhash: u64,
    /// `Some(reason)` when the document duplicates something already stored.
    pub duplicate: Option<RejectReason>,
}

/// Combined exact + near duplicate engine.
#[derive(Debug)]
pub struct DedupEngine {
    exact: Option<ExactDedup>,
    paragraphs: Option<ParagraphDeduper>,
    simhash: Option<SimHashIndex>,
    shingle_size: usize,
    max_hamming_distance: u32,
}

impl DedupEngine {
    /// Build the engine from the persistent tree handles.
    pub fn new(exact_tree: sled::Tree, simhash_tree: sled::Tree, config: &DedupConfig) -> Self {
        Self {
            exact: config.enable_exact.then(|| ExactDedup::new(exact_tree)),
            paragraphs: Some(ParagraphDeduper::new(
                config.paragraph_dedup_threshold,
                config.paragraph_dedup_max_entries,
            )),
            simhash: config
                .enable_near_dup
                .then(|| SimHashIndex::new(simhash_tree)),
            shingle_size: config.shingle_size.max(1),
            max_hamming_distance: config.max_hamming_distance,
        }
    }

    /// Strip repeated template paragraphs, then run exact and near duplicate checks.
    pub fn process(&self, text: &str) -> Result<ProcessedText> {
        let cleaned = match &self.paragraphs {
            Some(deduper) => deduper.dedup(text),
            None => text.to_string(),
        };

        let mut duplicate = None;
        if let Some(exact) = &self.exact {
            let digest = exact_digest(&cleaned);
            if exact.check_and_insert(&digest)? {
                duplicate = Some(RejectReason::ExactDuplicate);
            }
        }

        let fingerprint = simhash64(&cleaned, self.shingle_size);
        if duplicate.is_none() {
            if let Some(index) = &self.simhash {
                if index
                    .find_near(fingerprint, self.max_hamming_distance)?
                    .is_some()
                {
                    duplicate = Some(RejectReason::NearDuplicate);
                } else {
                    index.insert(fingerprint)?;
                }
            }
        }

        Ok(ProcessedText {
            text: cleaned,
            simhash: fingerprint,
            duplicate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(dir: &tempfile::TempDir) -> DedupEngine {
        let db = sled::Config::new()
            .path(dir.path())
            .temporary(true)
            .open()
            .unwrap();
        DedupEngine::new(
            db.open_tree("exact").unwrap(),
            db.open_tree("simhash").unwrap(),
            &DedupConfig::default(),
        )
    }

    const DOC_A: &str = "Rust guarantees memory safety without a garbage collector by tracking ownership at compile time. \
        The borrow checker rejects aliasing mutable references, which eliminates data races in safe code. \
        Cargo builds reproducible artifacts and resolves dependencies from a lockfile.";

    #[test]
    fn identical_documents_are_exact_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&dir);
        let first = engine.process(DOC_A).unwrap();
        assert_eq!(first.duplicate, None);
        let second = engine.process(DOC_A).unwrap();
        assert_eq!(second.duplicate, Some(RejectReason::ExactDuplicate));
    }

    #[test]
    fn different_documents_are_both_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&dir);
        assert_eq!(engine.process(DOC_A).unwrap().duplicate, None);
        let other = "Marine biology studies the behaviour of organisms that live in the ocean, \
            from microscopic plankton to enormous whales and deep sea corals.";
        assert_eq!(engine.process(other).unwrap().duplicate, None);
    }

    #[test]
    fn repeated_template_paragraphs_are_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let engine = engine(&dir);
        let template = "All rights reserved by Example Media Group. Terms of service apply.";
        let body = |n: usize| {
            format!(
                "# Article {n}\n\n{template}\n\nParagraph {n} explains a distinct topic with different wording each time.\n\n{template}"
            )
        };
        for n in 0..6 {
            let _ = engine.process(&body(n)).unwrap();
        }
        let processed = engine.process(&body(99)).unwrap();
        assert!(
            processed.text.matches(template).count() <= 1,
            "template not stripped: {}",
            processed.text
        );
    }
}
