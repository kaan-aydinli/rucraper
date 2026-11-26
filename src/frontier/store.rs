//! Persistent URL frontier, visited set and dedup indices on top of `sled`.
//!
//! The queue key is `[inverted_priority][sequence_be]`, so `first()` always
//! yields the highest-priority, oldest URL without any in-memory sorting.
//! Enqueue is race-free: it uses the visited tree's atomic
//! `compare_and_swap` as a claim, so two workers that discover the same link
//! cannot both push it.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use crate::error::{Result, ScraperError};

const TREE_PENDING: &str = "pending";
const TREE_VISITED: &str = "visited";
const TREE_META: &str = "meta";
const TREE_ROBOTS: &str = "robots";
const TREE_EXACT: &str = "exact_hashes";
const TREE_SIMHASH: &str = "simhash_lsh";
const KEY_SEQ: &[u8] = b"sequence";
const EMPTY: &[u8] = &[];

/// A queued URL plus the metadata the scheduler and extractor need.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UrlEntry {
    /// Normalized absolute URL.
    pub url: String,
    /// Registrable domain (eTLD+1) used for politeness accounting.
    pub domain: String,
    /// Distance from the seed (0 = seed).
    pub depth: u32,
    /// Higher values are dequeued first.
    pub priority: u8,
}

/// Result of trying to add a URL to the frontier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// The URL is new and is now queued.
    Inserted,
    /// The URL was seen before (visited set already contains it).
    Duplicate,
    /// The pending queue is at its configured budget.
    QueueFull,
}

/// Embedded state shared by the frontier, robots cache and dedup stages.
#[derive(Debug)]
pub struct Store {
    db: sled::Db,
    pending: sled::Tree,
    visited: sled::Tree,
    meta: sled::Tree,
    robots: sled::Tree,
    exact: sled::Tree,
    simhash: sled::Tree,
    next_seq: AtomicU64,
    queue_budget: usize,
}

impl Store {
    /// Open (or create) the database at `path`.
    pub fn open(path: &Path, queue_budget: usize) -> Result<Self> {
        std::fs::create_dir_all(path).map_err(|e| ScraperError::io_at(path, e))?;
        let db = sled::Config::new()
            .path(path)
            .cache_capacity(64 * 1024 * 1024)
            .flush_every_ms(Some(2_000))
            .open()
            .map_err(|e| ScraperError::storage(format!("cannot open {}: {e}", path.display())))?;
        let pending = db.open_tree(TREE_PENDING)?;
        let visited = db.open_tree(TREE_VISITED)?;
        let meta = db.open_tree(TREE_META)?;
        let robots = db.open_tree(TREE_ROBOTS)?;
        let exact = db.open_tree(TREE_EXACT)?;
        let simhash = db.open_tree(TREE_SIMHASH)?;
        let next_seq = recover_sequence(&pending, &meta);
        Ok(Self {
            db,
            pending,
            visited,
            meta,
            robots,
            exact,
            simhash,
            next_seq: AtomicU64::new(next_seq),
            queue_budget: queue_budget.max(1),
        })
    }

    /// Robots.txt cache tree (persistent across restarts).
    pub fn robots_tree(&self) -> sled::Tree {
        self.robots.clone()
    }

    /// Exact duplicate hash index.
    pub fn exact_tree(&self) -> sled::Tree {
        self.exact.clone()
    }

    /// SimHash LSH banding index.
    pub fn simhash_tree(&self) -> sled::Tree {
        self.simhash.clone()
    }

    /// Append a URL to the queue unless it was already visited or the queue is full.
    pub fn enqueue(&self, entry: &UrlEntry) -> Result<EnqueueOutcome> {
        if self.pending.len() >= self.queue_budget {
            return Ok(EnqueueOutcome::QueueFull);
        }
        let claim = xxh3_64(entry.url.as_bytes()).to_be_bytes();
        let claimed = self
            .visited
            .compare_and_swap(claim, None::<&[u8]>, Some(EMPTY))?;
        if claimed.is_err() {
            return Ok(EnqueueOutcome::Duplicate);
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let key = queue_key(entry.priority, seq);
        let value = serde_json::to_vec(entry)?;
        if let Err(error) = self.pending.insert(key, value) {
            // Roll the claim back so the URL can be retried later.
            let _ = self
                .visited
                .compare_and_swap(claim, Some(EMPTY), None::<&[u8]>);
            return Err(error.into());
        }
        Ok(EnqueueOutcome::Inserted)
    }

    /// Pop the best pending URL (highest priority, then oldest).
    pub fn pop_next(&self) -> Result<Option<UrlEntry>> {
        let Some((key, value)) = self.pending.first()? else {
            return Ok(None);
        };
        self.pending.remove(&key)?;
        match serde_json::from_slice::<UrlEntry>(&value) {
            Ok(entry) => Ok(Some(entry)),
            Err(error) => {
                tracing::warn!(key = ?key, %error, "dropping corrupt frontier entry");
                Ok(None)
            }
        }
    }

    /// Number of queued URLs.
    pub fn pending_count(&self) -> u64 {
        self.pending.len() as u64
    }

    /// Number of distinct URLs ever enqueued.
    pub fn visited_count(&self) -> u64 {
        self.visited.len() as u64
    }

    /// Persist everything (local shard finish / shutdown).
    pub fn flush(&self) -> Result<()> {
        let seq = self.next_seq.load(Ordering::Relaxed);
        self.meta.insert(KEY_SEQ, &seq.to_be_bytes())?;
        self.db
            .flush()
            .map_err(|e| ScraperError::storage(format!("sled flush failed: {e}")))?;
        Ok(())
    }
}

/// 9-byte ordering key: inverted priority followed by big-endian sequence.
fn queue_key(priority: u8, seq: u64) -> [u8; 9] {
    let mut key = [0u8; 9];
    key[0] = 255u8.saturating_sub(priority);
    key[1..].copy_from_slice(&seq.to_be_bytes());
    key
}

fn recover_sequence(pending: &sled::Tree, meta: &sled::Tree) -> u64 {
    let persisted = meta
        .get(KEY_SEQ)
        .ok()
        .flatten()
        .and_then(|bytes| bytes.as_ref().try_into().ok().map(u64::from_be_bytes))
        .unwrap_or(0);
    let from_queue = pending
        .last()
        .ok()
        .flatten()
        .and_then(|(key, _)| {
            let bytes: [u8; 8] = key.get(1..9)?.try_into().ok()?;
            Some(u64::from_be_bytes(bytes).saturating_add(1))
        })
        .unwrap_or(0);
    persisted.max(from_queue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), 1_000).expect("store");
        (dir, store)
    }

    fn entry(url: &str, priority: u8) -> UrlEntry {
        UrlEntry {
            url: url.to_string(),
            domain: "example.com".to_string(),
            depth: 1,
            priority,
        }
    }

    #[test]
    fn pops_higher_priority_first_and_is_fifo_within_priority() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.enqueue(&entry("https://example.com/a", 10)).unwrap(),
            EnqueueOutcome::Inserted
        );
        assert_eq!(
            store.enqueue(&entry("https://example.com/b", 200)).unwrap(),
            EnqueueOutcome::Inserted
        );
        assert_eq!(
            store.enqueue(&entry("https://example.com/c", 200)).unwrap(),
            EnqueueOutcome::Inserted
        );
        assert_eq!(
            store.pop_next().unwrap().unwrap().url,
            "https://example.com/b"
        );
        assert_eq!(
            store.pop_next().unwrap().unwrap().url,
            "https://example.com/c"
        );
        assert_eq!(
            store.pop_next().unwrap().unwrap().url,
            "https://example.com/a"
        );
        assert!(store.pop_next().unwrap().is_none());
    }

    #[test]
    fn duplicate_urls_are_never_queued_twice() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.enqueue(&entry("https://example.com/a", 1)).unwrap(),
            EnqueueOutcome::Inserted
        );
        assert_eq!(
            store.enqueue(&entry("https://example.com/a", 1)).unwrap(),
            EnqueueOutcome::Duplicate
        );
        assert_eq!(store.pending_count(), 1);
        assert_eq!(store.visited_count(), 1);
    }

    #[test]
    fn queue_budget_is_enforced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path(), 1).expect("store");
        assert_eq!(
            store.enqueue(&entry("https://example.com/a", 1)).unwrap(),
            EnqueueOutcome::Inserted
        );
        assert_eq!(
            store.enqueue(&entry("https://example.com/b", 1)).unwrap(),
            EnqueueOutcome::QueueFull
        );
    }

    #[test]
    fn state_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let store = Store::open(dir.path(), 100).expect("store");
            store.enqueue(&entry("https://example.com/a", 5)).unwrap();
            store.flush().unwrap();
        }
        let store = Store::open(dir.path(), 100).expect("reopen");
        assert_eq!(store.pending_count(), 1);
        assert_eq!(
            store.pop_next().unwrap().unwrap().url,
            "https://example.com/a"
        );
    }
}
