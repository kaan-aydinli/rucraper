//! 64-bit SimHash with a persistent 4-band LSH index.
//!
//! Shingles (overlapping word windows) are hashed with XXH3-64 and combined into
//! one 64-bit fingerprint. The fingerprint is split into four 16-bit bands; the
//! index key is `[band_index][band_value][fingerprint]`, so a lookup scans only
//! the bucket of each band (15 bytes per signature). A candidate is a near
//! duplicate when the Hamming distance between fingerprints is at most
//! `max_hamming_distance` (default 3). Four bands make the index robust for
//! that threshold: a 3-bit difference can only collide in the bucket of the
//! bands that contain no differing bit, which is why the lookup scans all four.

use xxhash_rust::xxh3::xxh3_64;

use crate::error::Result;

const BANDS: usize = 4;
const BAND_BYTES: usize = 2;
const BAND_WIDTH: u32 = 16;

/// Normalize a token for shingling: lowercase, alphanumeric only.
fn normalize_token(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_token)
        .filter(|token| !token.is_empty())
        .collect()
}

/// Compute the 64-bit SimHash of `text` using overlapping `shingle_size` windows.
pub fn simhash64(text: &str, shingle_size: usize) -> u64 {
    let words = tokens(text);
    let width = shingle_size.max(1);
    if words.is_empty() {
        return 0;
    }
    let mut accumulator = [0i32; 64];
    if words.len() < width {
        for word in &words {
            add_hash(&mut accumulator, xxh3_64(word.as_bytes()));
        }
    } else {
        for window in words.windows(width) {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for word in window {
                std::hash::Hash::hash(word, &mut hasher);
            }
            let seed = std::hash::Hasher::finish(&hasher);
            add_hash(&mut accumulator, xxh3_64(&seed.to_be_bytes()));
        }
    }
    let mut fingerprint = 0u64;
    for (bit, weight) in accumulator.iter().enumerate() {
        if *weight > 0 {
            fingerprint |= 1u64 << bit;
        }
    }
    fingerprint
}

fn add_hash(accumulator: &mut [i32; 64], hash: u64) {
    for (bit, weight) in accumulator.iter_mut().enumerate() {
        if hash & (1u64 << bit) != 0 {
            *weight += 1;
        } else {
            *weight -= 1;
        }
    }
}

/// Hamming distance between two fingerprints.
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Persistent LSH index of SimHash fingerprints.
#[derive(Debug)]
pub struct SimHashIndex {
    tree: sled::Tree,
}

impl SimHashIndex {
    /// Wrap an existing tree.
    pub fn new(tree: sled::Tree) -> Self {
        Self { tree }
    }

    /// Find the closest stored fingerprint within `max_distance`.
    pub fn find_near(&self, fingerprint: u64, max_distance: u32) -> Result<Option<u64>> {
        let mut best: Option<(u32, u64)> = None;
        for band in 0..BANDS {
            let prefix = band_prefix(band, fingerprint);
            for item in self.tree.scan_prefix(&prefix) {
                let (key, _) = item?;
                if key.len() < 8 {
                    continue;
                }
                let stored = fingerprint_from_key(&key);
                let distance = hamming_distance(fingerprint, stored);
                if distance <= max_distance {
                    match best {
                        Some((current, _)) if current <= distance => {}
                        _ => best = Some((distance, stored)),
                    }
                }
            }
        }
        Ok(best.map(|(_, stored)| stored))
    }

    /// Store a fingerprint in all bands.
    pub fn insert(&self, fingerprint: u64) -> Result<()> {
        let value = fingerprint.to_be_bytes();
        for band in 0..BANDS {
            let mut key = band_prefix(band, fingerprint);
            key.extend_from_slice(&value);
            self.tree.insert(key, &[][..])?;
        }
        Ok(())
    }

    /// Number of band entries stored.
    pub fn len(&self) -> u64 {
        self.tree.len() as u64
    }

    /// True when no fingerprint is indexed.
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }
}

fn band_value(band: usize, fingerprint: u64) -> u16 {
    let shift = 64 - (band as u32 + 1) * BAND_WIDTH;
    ((fingerprint >> shift) & 0xFFFF) as u16
}

fn band_prefix(band: usize, fingerprint: u64) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(1 + BAND_BYTES);
    prefix.push(band as u8);
    prefix.extend_from_slice(&band_value(band, fingerprint).to_be_bytes());
    prefix
}

fn fingerprint_from_key(key: &[u8]) -> u64 {
    let start = key.len().saturating_sub(8);
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&key[start..]);
    u64::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "Rust is a systems programming language focused on safety, speed and concurrency. \
        Its ownership model eliminates whole classes of memory errors without a garbage collector. \
        The borrow checker validates references at compile time, preventing data races and use after free bugs. \
        Cargo builds reproducible artifacts and manages dependencies from a lockfile. \
        Async runtimes schedule many lightweight tasks on a small thread pool. \
        Error handling is explicit and typesafe through the Result enum.";

    const DIFFERENT: &str = "Marine biology studies organisms that live in the ocean, from microscopic plankton to enormous whales. \
        Researchers tag migratory species to understand their routes across the Pacific. \
        Coral reefs support a quarter of all marine species while covering a tiny fraction of the sea floor. \
        Conservation programs restore damaged habitats and monitor water quality over decades.";

    /// Deterministic full length article (~5,500 words).
    ///
    /// A 64-bit SimHash with a Hamming threshold of 3 is calibrated for
    /// near-identical documents: each bit is a noisy majority vote, so the
    /// fixture must be long enough that a one-token edit perturbs only a handful
    /// of shingles. Short documents simply do not have enough features for that
    /// guarantee, which is exactly why real crawlers also keep the exact index.
    fn large_article(year: u16) -> String {
        use std::fmt::Write as _;

        const SUBJECTS: [&str; 12] = [
            "The allocator",
            "The scheduler",
            "The parser",
            "The runtime",
            "The compiler",
            "The linker",
            "The profiler",
            "The debugger",
            "The storage layer",
            "The network stack",
            "The validator",
            "The indexer",
        ];
        const VERBS: [&str; 12] = [
            "validates",
            "optimises",
            "records",
            "batches",
            "tracks",
            "streams",
            "reorders",
            "verifies",
            "compacts",
            "measures",
            "replays",
            "annotates",
        ];
        const OBJECTS: [&str; 12] = [
            "memory pages",
            "symbol tables",
            "socket buffers",
            "lock free queues",
            "stack frames",
            "cache lines",
            "task graphs",
            "error reports",
            "shard manifests",
            "hash buckets",
            "retry budgets",
            "domain schedules",
        ];

        let mut text = String::with_capacity(64 * 1024);
        for index in 0..200usize {
            let _ = write!(
                text,
                "{} {} {} while handling case {} around second {} of the run, and the team recorded the outcome for later review. ",
                SUBJECTS[index % SUBJECTS.len()],
                VERBS[(index * 5) % VERBS.len()],
                OBJECTS[(index * 7) % OBJECTS.len()],
                index,
                index * 3 + 1
            );
        }
        let _ = write!(text, "The notes were published in {year}.");
        text
    }

    #[test]
    fn identical_text_produces_identical_fingerprints() {
        assert_eq!(simhash64(ORIGINAL, 5), simhash64(ORIGINAL, 5));
    }

    #[test]
    fn small_edits_are_near_duplicates() {
        // Over 99% of the document is untouched; only the publication date differs.
        let original = large_article(2025);
        let modified = large_article(2026);
        let original_hash = simhash64(&original, 5);
        let modified_hash = simhash64(&modified, 5);
        let distance = hamming_distance(original_hash, modified_hash);
        assert!(distance <= 3, "distance {distance} too large");

        let dir = tempfile::tempdir().unwrap();
        let db = sled::Config::new()
            .path(dir.path())
            .temporary(true)
            .open()
            .unwrap();
        let index = SimHashIndex::new(db.open_tree("simhash").unwrap());
        index.insert(original_hash).unwrap();
        assert_eq!(
            index.find_near(modified_hash, 3).unwrap(),
            Some(original_hash)
        );
    }

    #[test]
    fn unrelated_text_is_not_a_near_duplicate() {
        let original_hash = simhash64(ORIGINAL, 5);
        let different_hash = simhash64(DIFFERENT, 5);
        let distance = hamming_distance(original_hash, different_hash);
        assert!(distance > 3, "distance {distance} unexpectedly small");

        let dir = tempfile::tempdir().unwrap();
        let db = sled::Config::new()
            .path(dir.path())
            .temporary(true)
            .open()
            .unwrap();
        let index = SimHashIndex::new(db.open_tree("simhash").unwrap());
        index.insert(original_hash).unwrap();
        assert_eq!(index.find_near(different_hash, 3).unwrap(), None);
    }

    #[test]
    fn index_is_persistent_and_stores_four_bands() {
        let dir = tempfile::tempdir().unwrap();
        let fingerprint = simhash64(ORIGINAL, 5);
        {
            let db = sled::Config::new().path(dir.path()).open().unwrap();
            let index = SimHashIndex::new(db.open_tree("simhash").unwrap());
            index.insert(fingerprint).unwrap();
            assert_eq!(index.len(), 4);
        }
        let db = sled::Config::new().path(dir.path()).open().unwrap();
        let index = SimHashIndex::new(db.open_tree("simhash").unwrap());
        assert_eq!(index.find_near(fingerprint, 0).unwrap(), Some(fingerprint));
    }

    #[test]
    fn hamming_distance_counts_bits() {
        assert_eq!(hamming_distance(0, 0), 0);
        assert_eq!(hamming_distance(0b1011, 0b1001), 1);
        assert_eq!(hamming_distance(u64::MAX, 0), 64);
    }

    #[test]
    fn empty_text_hashes_to_zero() {
        assert_eq!(simhash64("", 5), 0);
        assert_eq!(simhash64("   ", 5), 0);
    }
}
