//! Lock-free counters plus an `indicatif` multi-progress dashboard.
//!
//! Every stage owns an `Arc<Stats>` and only ever performs relaxed atomic
//! increments: no mutex is taken on the hot path, so a thousand concurrent
//! workers cannot serialise on telemetry. The dashboard is a separate task that
//! *reads* those atomics a few times per second.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

use crate::filter::RejectReason;

/// Dispatch/completion bookkeeping shared with `CompletionGuard`.
#[derive(Debug, Default)]
pub struct Progress {
    dispatched: AtomicU64,
    completed: AtomicU64,
}

impl Progress {
    /// Number of URLs handed to the pipeline.
    pub fn dispatched(&self) -> u64 {
        self.dispatched.load(Ordering::Relaxed)
    }

    /// Number of URLs fully drained by the pipeline.
    pub fn completed(&self) -> u64 {
        self.completed.load(Ordering::Relaxed)
    }

    /// Register one new in-flight unit of work.
    pub fn dispatch(&self) {
        self.dispatched.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drop guard that marks exactly one unit of work as finished.
///
/// The guard travels with the payload through all channels, so an item that is
/// rejected, skipped, panics inside a blocking task or is written to a shard all
/// count their completion precisely once. This is what lets the frontier feeder
/// detect a drained crawl without brittle polling of every stage.
#[derive(Debug)]
pub struct CompletionGuard {
    progress: Arc<Progress>,
}

impl CompletionGuard {
    /// Create a guard that reports into `progress` when dropped.
    pub fn new(progress: Arc<Progress>) -> Self {
        Self { progress }
    }
}

impl Drop for CompletionGuard {
    fn drop(&mut self) {
        self.progress.completed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Lock-free pipeline metrics.
#[derive(Debug)]
pub struct Stats {
    started: Instant,
    fetched: AtomicU64,
    fetch_errors: AtomicU64,
    robots_blocked: AtomicU64,
    extracted: AtomicU64,
    accepted_docs: AtomicU64,
    accepted_words: AtomicU64,
    accepted_chars: AtomicU64,
    accepted_bytes: AtomicU64,
    compressed_bytes: AtomicU64,
    records: AtomicU64,
    dedup_removed: AtomicU64,
    shards: AtomicU64,
    queue_depth: AtomicU64,
    visited: AtomicU64,
    rejected: Vec<AtomicU64>,
}

/// Point-in-time view of [`Stats`], cheap to serialize for manifests and reports.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsSnapshot {
    /// Pages fetched with HTTP 2xx and an HTML body.
    pub fetched: u64,
    /// Fetch attempts that ended in an error.
    pub fetch_errors: u64,
    /// URLs skipped because robots.txt disallowed them.
    pub robots_blocked: u64,
    /// Documents that survived extraction.
    pub extracted: u64,
    /// Documents written to a shard.
    pub accepted_docs: u64,
    /// Total words written.
    pub accepted_words: u64,
    /// Total characters written.
    pub accepted_chars: u64,
    /// Uncompressed JSONL bytes written.
    pub accepted_bytes: u64,
    /// Compressed shard bytes on disk.
    pub compressed_bytes: u64,
    /// Records written (topic blocks in `txt` mode, one per document in `jsonl`).
    pub records: u64,
    /// Duplicate blocks, paragraphs and sentences removed before writing.
    pub dedup_removed: u64,
    /// Finished shards.
    pub shards: u64,
    /// Pending URLs in the frontier at the time of the snapshot.
    pub queue_depth: u64,
    /// Distinct URLs ever enqueued.
    pub visited: u64,
    /// Rejections keyed by their stable reason code.
    pub rejects: Vec<(String, u64)>,
    /// Wall clock duration of the run in seconds.
    pub elapsed_secs: f64,
}

impl Stats {
    /// Create a zeroed counter set.
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            fetched: AtomicU64::new(0),
            fetch_errors: AtomicU64::new(0),
            robots_blocked: AtomicU64::new(0),
            extracted: AtomicU64::new(0),
            accepted_docs: AtomicU64::new(0),
            accepted_words: AtomicU64::new(0),
            accepted_chars: AtomicU64::new(0),
            accepted_bytes: AtomicU64::new(0),
            compressed_bytes: AtomicU64::new(0),
            records: AtomicU64::new(0),
            dedup_removed: AtomicU64::new(0),
            shards: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            visited: AtomicU64::new(0),
            rejected: RejectReason::ALL
                .iter()
                .map(|_| AtomicU64::new(0))
                .collect(),
        }
    }

    /// Record a successfully fetched HTML page.
    pub fn record_fetch_ok(&self) {
        self.fetched.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed fetch.
    pub fn record_fetch_error(&self) {
        self.fetch_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a robots.txt rejection.
    pub fn record_robots_blocked(&self) {
        self.robots_blocked.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that extraction produced a non-empty body.
    pub fn record_extracted(&self) {
        self.extracted.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an accepted (written) document.
    pub fn record_accepted(&self, words: u64, chars: u64, json_bytes: u64) {
        self.accepted_docs.fetch_add(1, Ordering::Relaxed);
        self.accepted_words.fetch_add(words, Ordering::Relaxed);
        self.accepted_chars.fetch_add(chars, Ordering::Relaxed);
        self.accepted_bytes.fetch_add(json_bytes, Ordering::Relaxed);
    }

    /// Record one finished shard and its compressed size.
    pub fn record_shard(&self, compressed_bytes: u64) {
        self.shards.fetch_add(1, Ordering::Relaxed);
        self.compressed_bytes
            .fetch_add(compressed_bytes, Ordering::Relaxed);
    }

    /// Record `count` written records (topic blocks).
    pub fn record_records(&self, count: u64) {
        self.records.fetch_add(count, Ordering::Relaxed);
    }

    /// Record `count` duplicate units removed by the corpus deduper.
    pub fn record_dedup(&self, count: u64) {
        self.dedup_removed.fetch_add(count, Ordering::Relaxed);
    }

    /// Record a rejection reason.
    pub fn record_reject(&self, reason: RejectReason) {
        if let Some(counter) = self.rejected.get(reason.index()) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Publish the current pending-queue depth.
    pub fn set_queue_depth(&self, depth: u64) {
        self.queue_depth.store(depth, Ordering::Relaxed);
    }

    /// Publish the current visited-set size.
    pub fn set_visited(&self, visited: u64) {
        self.visited.store(visited, Ordering::Relaxed);
    }

    /// Read a single rejection counter.
    pub fn reject_count(&self, reason: RejectReason) -> u64 {
        self.rejected
            .get(reason.index())
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Snapshot everything for reporting.
    pub fn snapshot(&self) -> StatsSnapshot {
        let mut rejects = Vec::with_capacity(RejectReason::ALL.len());
        for reason in RejectReason::ALL {
            let count = self.reject_count(*reason);
            rejects.push((reason.as_str().to_string(), count));
        }
        StatsSnapshot {
            fetched: self.fetched.load(Ordering::Relaxed),
            fetch_errors: self.fetch_errors.load(Ordering::Relaxed),
            robots_blocked: self.robots_blocked.load(Ordering::Relaxed),
            extracted: self.extracted.load(Ordering::Relaxed),
            accepted_docs: self.accepted_docs.load(Ordering::Relaxed),
            accepted_words: self.accepted_words.load(Ordering::Relaxed),
            accepted_chars: self.accepted_chars.load(Ordering::Relaxed),
            accepted_bytes: self.accepted_bytes.load(Ordering::Relaxed),
            compressed_bytes: self.compressed_bytes.load(Ordering::Relaxed),
            records: self.records.load(Ordering::Relaxed),
            dedup_removed: self.dedup_removed.load(Ordering::Relaxed),
            shards: self.shards.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
            visited: self.visited.load(Ordering::Relaxed),
            rejects,
            elapsed_secs: self.started.elapsed().as_secs_f64(),
        }
    }

    /// Rejection totals keyed by reason (only non-zero entries).
    pub fn reject_breakdown(&self) -> Vec<(String, u64)> {
        RejectReason::ALL
            .iter()
            .map(|r| (r.as_str().to_string(), self.reject_count(*r)))
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    /// Human readable one-line summary of the counters.
    pub fn summary_line(&self) -> String {
        let snap = self.snapshot();
        let mut line = String::new();
        let _ = write!(
            line,
            "fetched={} errors={} extracted={} docs={} words={} shards={}",
            snap.fetched,
            snap.fetch_errors,
            snap.extracted,
            snap.accepted_docs,
            snap.accepted_words,
            snap.shards
        );
        line
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

/// Live terminal dashboard backed by a hidden or visible `MultiProgress`.
pub struct Dashboard {
    multi: MultiProgress,
    header: ProgressBar,
    stages: Vec<(&'static str, ProgressBar)>,
}

impl Dashboard {
    /// Build a dashboard; `enabled == false` creates hidden bars so the code
    /// path is identical in tests and in non-interactive environments.
    pub fn new(enabled: bool) -> Self {
        let multi = if enabled {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden())
        };
        let header = multi.add(ProgressBar::new_spinner());
        header.set_style(spinner_style("{spinner} {msg}"));
        let labels: [&'static str; 5] = ["frontier", "fetch", "extract", "filter", "dedup"];
        let mut stages = Vec::with_capacity(labels.len());
        for label in labels {
            let bar = multi.add(ProgressBar::new_spinner());
            bar.set_style(spinner_style("{spinner} {msg}"));
            bar.enable_steady_tick(Duration::from_millis(250));
            stages.push((label, bar));
        }
        Self {
            multi,
            header,
            stages,
        }
    }

    /// Refresh every line from the current counters.
    pub fn render(&self, stats: &Stats, progress: &Progress) {
        let snap = stats.snapshot();
        self.header.set_message(format!(
            "{} | in-flight {} | queue {} | visited {} | {:.2} MiB out",
            stats.summary_line(),
            progress.dispatched().saturating_sub(progress.completed()),
            snap.queue_depth,
            snap.visited,
            snap.accepted_bytes as f64 / (1024.0 * 1024.0)
        ));
        for (label, bar) in &self.stages {
            let message = match *label {
                "frontier" => format!(
                    "frontier   queued={} visited={} dispatched={}",
                    snap.queue_depth,
                    snap.visited,
                    progress.dispatched()
                ),
                "fetch" => format!(
                    "fetch      ok={} errors={} robots_blocked={}",
                    snap.fetched, snap.fetch_errors, snap.robots_blocked
                ),
                "extract" => format!(
                    "extract    ok={} rejected={}",
                    snap.extracted,
                    short_rejects(
                        stats,
                        &[RejectReason::EmptyContent, RejectReason::TooShortContent,]
                    )
                ),
                "filter" => format!(
                    "filter     accepted={} rejected={}",
                    snap.accepted_docs,
                    short_rejects(stats, FILTER_REJECTS)
                ),
                _ => format!(
                    "dedup      exact_dup={} near_dup={} shards={}",
                    stats.reject_count(RejectReason::ExactDuplicate),
                    stats.reject_count(RejectReason::NearDuplicate),
                    snap.shards
                ),
            };
            bar.set_message(message);
        }
    }

    /// Stop ticking and clear the transient lines.
    pub fn finish(&self) {
        for (_, bar) in &self.stages {
            bar.finish_and_clear();
        }
        self.header.finish_and_clear();
        let _ = &self.multi;
    }
}

/// Rejections produced by the quality filter itself (fetch, robots and dedup
/// have their own counters and must not be mixed into the filter line).
const FILTER_REJECTS: &[RejectReason] = &[
    RejectReason::TooFewWords,
    RejectReason::TooManyWords,
    RejectReason::AvgWordLength,
    RejectReason::LowPunctuation,
    RejectReason::LowAlphaWords,
    RejectReason::HighSymbolRatio,
    RejectReason::BlockedPhrase,
    RejectReason::DuplicateLines,
    RejectReason::RepetitiveNgrams,
    RejectReason::LanguageUndetermined,
    RejectReason::LowLanguageConfidence,
    RejectReason::UnsupportedLanguage,
];

fn short_rejects(stats: &Stats, reasons: &[RejectReason]) -> u64 {
    reasons.iter().map(|r| stats.reject_count(*r)).sum()
}

fn spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
        .tick_chars("⠁⠂⠄⡀⠄⠂")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_lock_free_and_correct() {
        let stats = Stats::new();
        stats.record_fetch_ok();
        stats.record_fetch_error();
        stats.record_reject(RejectReason::TooFewWords);
        stats.record_reject(RejectReason::TooFewWords);
        stats.record_accepted(100, 700, 800);
        let snap = stats.snapshot();
        assert_eq!(snap.fetched, 1);
        assert_eq!(snap.fetch_errors, 1);
        assert_eq!(snap.accepted_docs, 1);
        assert_eq!(snap.accepted_words, 100);
        assert_eq!(stats.reject_count(RejectReason::TooFewWords), 2);
        assert!(snap
            .rejects
            .iter()
            .any(|(k, v)| k == "too_few_words" && *v == 2));
    }

    #[test]
    fn completion_guard_marks_completion_once() {
        let progress = Arc::new(Progress::default());
        progress.dispatch();
        {
            let _guard = CompletionGuard::new(progress.clone());
        }
        assert_eq!(progress.dispatched(), 1);
        assert_eq!(progress.completed(), 1);
    }

    #[test]
    fn hidden_dashboard_does_not_panic() {
        let stats = Stats::new();
        let progress = Progress::default();
        let dash = Dashboard::new(false);
        dash.render(&stats, &progress);
        dash.finish();
    }
}
