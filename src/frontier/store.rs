//! Persistent URL frontier, visited set and dedup indices on top of `sled`.
//!
//! The queue key is `[inverted_priority][sequence_be]`, so `first()` always
//! yields the highest-priority, oldest URL without any in-memory sorting.
//! Enqueue is race-free: it uses the visited tree's atomic
//! `compare_and_swap` as a claim, so two workers that discover the same link
//! cannot both push it.
//!
//! Queue and visited sizes are kept in atomics. `sled::Tree::len()` walks the
//! whole tree, so calling it on every enqueue made discovery quadratic in the
//! frontier size; the counters are now read once at open and updated on each
//! mutation instead.

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
    pending_count: AtomicU64,
    visited_count: AtomicU64,
    queue_budget: u64,
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
        // One O(n) pass at open; every later read is a single atomic load.
        let pending_count = pending.len() as u64;
        let visited_count = visited.len() as u64;
        Ok(Self {
            db,
            pending,
            visited,
            meta,
            robots,
            exact,
            simhash,
            next_seq: AtomicU64::new(next_seq),
            pending_count: AtomicU64::new(pending_count),
            visited_count: AtomicU64::new(visited_count),
            queue_budget: queue_budget.max(1) as u64,
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
        if self.pending_count.load(Ordering::Relaxed) >= self.queue_budget {
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
        self.pending_count.fetch_add(1, Ordering::Relaxed);
        self.visited_count.fetch_add(1, Ordering::Relaxed);
        Ok(EnqueueOutcome::Inserted)
    }

    /// Pop the best pending URL (highest priority, then oldest).
    pub fn pop_next(&self) -> Result<Option<UrlEntry>> {
        Ok(self.pop_batch(1)?.pop())
    }

    /// Pop up to `limit` URLs in one blocking round trip.
    ///
    /// The feeder used to call `pop_next` through `spawn_blocking` once per
    /// URL, which made the frontier hand-off the slowest hop of the pipeline.
    /// Batching amortises the task switch and the first/remove calls over many
    /// URLs, so a fast fetch stage is no longer starved by the feeder.
    pub fn pop_batch(&self, limit: usize) -> Result<Vec<UrlEntry>> {
        let mut entries = Vec::with_capacity(limit.min(256));
        for _ in 0..limit {
            let Some((key, value)) = self.pending.first()? else {
                break;
            };
            self.pending.remove(&key)?;
            self.decrement_pending();
            match serde_json::from_slice::<UrlEntry>(&value) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    tracing::warn!(key = ?key, %error, "dropping corrupt frontier entry");
                }
            }
        }
        Ok(entries)
    }

    /// Put already-claimed entries back at the queue tail.
    ///
    /// Used when a crawl stops early (shutdown signal, page budget, closed
    /// channel): the URLs were popped and would otherwise be lost, while their
    /// visited claim stays valid so they are never enqueued twice.
    pub fn requeue(&self, entries: &[UrlEntry]) -> Result<()> {
        let mut requeued = 0u64;
        for entry in entries {
            let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
            let key = queue_key(entry.priority, seq);
            match serde_json::to_vec(entry) {
                Ok(value) => {
                    if let Err(error) = self.pending.insert(key, value) {
                        tracing::warn!(%error, "cannot requeue frontier entry");
                    } else {
                        requeued += 1;
                    }
                }
                Err(error) => tracing::warn!(%error, "cannot encode frontier entry"),
            }
        }
        if requeued > 0 {
            self.pending_count.fetch_add(requeued, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Number of queued URLs.
    pub fn pending_count(&self) -> u64 {
        self.pending_count.load(Ordering::Relaxed)
    }

    /// Number of distinct URLs ever enqueued.
    pub fn visited_count(&self) -> u64 {
        self.visited_count.load(Ordering::Relaxed)
    }

    /// Saturating decrement so a counter can never wrap below zero.
    ///
    /// A compare-exchange loop is used instead of `fetch_update`/`try_update`
    /// because that method was renamed in Rust 1.100 while the crate still
    /// supports 1.88.
    fn decrement_pending(&self) {
        let mut current = self.pending_count.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(1);
            match self.pending_count.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
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
    fn batch_pop_is_priority_ordered_and_keeps_counts_exact() {
        let (_dir, store) = temp_store();
        for index in 0..10u8 {
            let priority = if index % 2 == 0 { 50 } else { 200 };
            store
                .enqueue(&entry(&format!("https://example.com/{index}"), priority))
                .unwrap();
        }
        assert_eq!(store.pending_count(), 10);

        let batch = store.pop_batch(4).unwrap();
        assert_eq!(batch.len(), 4);
        assert!(
            batch.iter().all(|entry| entry.priority == 200),
            "the batch must start with the highest priorities"
        );
        assert_eq!(store.pending_count(), 6);
        assert_eq!(store.visited_count(), 10);

        let rest = store.pop_batch(100).unwrap();
        assert_eq!(rest.len(), 6);
        assert_eq!(store.pending_count(), 0);
        assert!(store.pop_batch(4).unwrap().is_empty());
    }

    #[test]
    fn requeue_restores_popped_entries_without_reclaiming_visited() {
        let (_dir, store) = temp_store();
        let first = entry("https://example.com/a", 10);
        store.enqueue(&first).unwrap();
        let popped = store.pop_batch(1).unwrap();
        assert_eq!(popped.len(), 1);
        assert_eq!(store.pending_count(), 0);

        store.requeue(&popped).unwrap();
        assert_eq!(store.pending_count(), 1);
        assert_eq!(store.visited_count(), 1, "requeue must not add a new claim");
        assert_eq!(store.pop_next().unwrap().unwrap().url, first.url);
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
        assert_eq!(store.visited_count(), 1);
        assert_eq!(
            store.pop_next().unwrap().unwrap().url,
            "https://example.com/a"
        );
        assert_eq!(store.pending_count(), 0);
        assert_eq!(
            store.visited_count(),
            1,
            "popping must not forget the claim"
        );
    }
}
