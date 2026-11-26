//! Streaming pipeline orchestration.
//!
//! ```text
//! frontier (LSM) -> [fetch] -> [extract] -> [filter] -> [dedup] -> [shard writer]
//! ```
//!
//! Every arrow is a bounded `tokio::sync::mpsc` channel, so a slow stage applies
//! backpressure to the whole crawl instead of growing an unbounded queue. CPU
//! stages run their per-item work inside `spawn_blocking` with a semaphore that
//! caps concurrency, which keeps the async runtime responsive and the number of
//! in-flight blocking tasks fixed.
//!
//! Completion tracking uses a drop guard that travels with each item: when the
//! guard is dropped (written, rejected, skipped or failed) the item is counted as
//! finished. The frontier feeder stops when the queue is empty *and* every
//! dispatched URL has finished, which is what makes the run terminate cleanly
//! without any polling of individual stages.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;
use url::Url;

use crate::config::Config;
use crate::dedup::DedupEngine;
use crate::error::{Result, ScraperError};
use crate::export::{clean_title, sanitize_markdown};
use crate::extractor;
use crate::fetcher::{FetchFailureKind, FetchedPage, Fetcher};
use crate::filter::{self, CleanDocument, RejectReason};
use crate::frontier::{
    domain_permitted, priority_for_depth, registrable_domain, DomainScheduler, EnqueueOutcome,
    RobotsCache, Store, UrlEntry,
};
use crate::storage::{Document, SavedUrls, ShardWriter};
use crate::telemetry::{CompletionGuard, Dashboard, Progress, Stats, StatsSnapshot};

static SITEMAP_LOC: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?is)<loc>\s*([^<\s]+)\s*</loc>").expect("static regex: sitemap loc")
});

/// What the pipeline should feed the cleaning stages with.
#[derive(Debug, Clone)]
pub enum RunMode {
    /// Fetch pages over HTTP starting from `seeds` and follow links.
    Crawl {
        /// Seed URLs read from the CLI (absolute http/https URLs).
        seeds: Vec<String>,
    },
    /// Clean files that are already on disk; no network access at all.
    CleanDirectory {
        /// Directory with `.html`, `.htm`, `.xhtml`, `.jsonl` or `.jsonl.zst` files.
        input_dir: PathBuf,
    },
}

impl RunMode {
    /// True for network crawls.
    pub fn is_crawl(&self) -> bool {
        matches!(self, RunMode::Crawl { .. })
    }
}

/// Outcome of a completed run.
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// Final counter snapshot.
    pub snapshot: StatsSnapshot,
    /// Manifest of the produced dataset.
    pub manifest_path: PathBuf,
}

#[derive(Clone)]
struct StageContext {
    config: Arc<Config>,
    store: Arc<Store>,
    stats: Arc<Stats>,
    crawl: bool,
}

struct FetchJob {
    entry: UrlEntry,
    _guard: CompletionGuard,
}

struct RawPage {
    url: String,
    domain: String,
    html: String,
    depth: u32,
    scraped_at: DateTime<Utc>,
    _guard: CompletionGuard,
}

struct ExtractedItem {
    url: String,
    domain: String,
    title: String,
    text: String,
    scraped_at: DateTime<Utc>,
    _guard: CompletionGuard,
}

struct CleanItem {
    url: String,
    domain: String,
    title: String,
    clean: CleanDocument,
    scraped_at: DateTime<Utc>,
    _guard: CompletionGuard,
}

struct AcceptedItem {
    doc: Document,
    _guard: CompletionGuard,
}

enum FetchOutcome {
    Page(FetchedPage),
    Skipped(RejectReason),
}

/// Run the whole pipeline until the frontier is drained or a shutdown signal arrives.
pub async fn run(config: Config, mode: RunMode) -> Result<RunSummary> {
    config.validate()?;
    config.prepare_directories()?;

    let crawl = mode.is_crawl();
    let config = Arc::new(config);
    let store = Arc::new(Store::open(
        &config.general.db_path,
        config.frontier.queue_capacity,
    )?);
    let stats = Arc::new(Stats::new());
    let progress = Arc::new(Progress::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    // Pages saved by earlier runs live in a portable URL file; loading it turns
    // "already in the corpus" into a pre-fetch skip, even with a fresh database.
    let saved_urls = if config.output.saved_urls_file.trim().is_empty() {
        None
    } else {
        Some(Arc::new(Mutex::new(SavedUrls::open(
            &config.general.output_dir,
            &config.output.saved_urls_file,
        )?)))
    };
    if let Some(saved) = &saved_urls {
        let saved = saved.lock().expect("saved url index poisoned");
        tracing::info!(
            path = %saved.path().display(),
            saved_pages = saved.len(),
            "loaded saved content-page urls"
        );
    }
    stats.set_visited(store.visited_count());
    stats.set_queue_depth(store.pending_count());

    let ctx = StageContext {
        config: config.clone(),
        store: store.clone(),
        stats: stats.clone(),
        crawl,
    };

    let fetcher = Fetcher::new(&config.fetch)?;
    let robots = Arc::new(RobotsCache::new(
        fetcher.client(),
        &config.fetch.user_agent,
        config.frontier.robots_ttl_secs,
        store.robots_tree(),
    ));
    let scheduler = Arc::new(DomainScheduler::new(
        config.frontier.max_concurrency_per_domain,
        Duration::from_millis(config.frontier.min_delay_ms),
    ));

    if let RunMode::Crawl { seeds } = &mode {
        if seeds.is_empty() && store.pending_count() == 0 {
            return Err(ScraperError::config(
                "no seeds were given and the frontier is empty; set frontier.allowed_domains_file, pass --seeds <file>, or seed the frontier first",
            ));
        }
        let added = seed_frontier(&ctx, &robots, &fetcher, seeds, &saved_urls).await?;
        tracing::info!(
            added,
            pending = store.pending_count(),
            "frontier seeded (already queued URLs are kept)"
        );
    }

    let capacity = config.pipeline.channel_capacity.max(1);
    let (jobs_tx, jobs_rx) = mpsc::channel::<FetchJob>(capacity);
    let (raw_tx, raw_rx) = mpsc::channel::<RawPage>(capacity);
    let (extracted_tx, extracted_rx) = mpsc::channel::<ExtractedItem>(capacity);
    let (clean_tx, clean_rx) = mpsc::channel::<CleanItem>(capacity);
    let (doc_tx, doc_rx) = mpsc::channel::<AcceptedItem>(capacity);

    let dashboard = Arc::new(Dashboard::new(config.general.progress));
    let stop_ticker = Arc::new(AtomicBool::new(false));
    let ticker = spawn_dashboard(
        dashboard.clone(),
        stats.clone(),
        progress.clone(),
        stop_ticker.clone(),
        config.pipeline.dashboard_interval_ms,
    );
    let signal_watcher = spawn_signal_watcher(shutdown.clone());

    let writer_handle = tokio::spawn(writer_stage(
        doc_rx,
        config.clone(),
        stats.clone(),
        shutdown.clone(),
        saved_urls.clone(),
    ));

    let dedup_engine = Arc::new(DedupEngine::new(
        store.exact_tree(),
        store.simhash_tree(),
        &config.dedup,
    ));
    let dedup_handle = tokio::spawn(cpu_stage(
        clean_rx,
        doc_tx,
        config.pipeline.resolved_dedup_workers(),
        {
            let ctx = ctx.clone();
            let engine = dedup_engine.clone();
            move |item: CleanItem| dedup_work(&ctx, &engine, item)
        },
    ));

    let filter_handle = tokio::spawn(cpu_stage(
        extracted_rx,
        clean_tx,
        config.pipeline.resolved_filter_workers(),
        {
            let ctx = ctx.clone();
            move |item: ExtractedItem| filter_work(&ctx, item)
        },
    ));

    let extract_handle = tokio::spawn(cpu_stage(
        raw_rx,
        extracted_tx,
        config.pipeline.resolved_extract_workers(),
        {
            let ctx = ctx.clone();
            move |page: RawPage| extract_work(&ctx, page)
        },
    ));

    // The fetch stage only exists for network crawls: in `clean-html` mode the
    // feeder writes straight into the extraction channel, so holding a fetch
    // task (and its channel handles) open would keep the pipeline alive forever.
    let fetch_handle = if crawl {
        tokio::spawn(fetch_stage(
            jobs_rx,
            raw_tx.clone(),
            ctx.clone(),
            fetcher.clone(),
            scheduler.clone(),
            robots.clone(),
            shutdown.clone(),
        ))
    } else {
        drop(jobs_rx);
        drop(scheduler);
        tokio::spawn(async { Ok(()) })
    };

    let feed_result = match &mode {
        RunMode::Crawl { .. } => {
            drop(raw_tx);
            feed_crawl(
                ctx.clone(),
                progress.clone(),
                jobs_tx,
                shutdown.clone(),
                saved_urls.clone(),
            )
            .await
        }
        RunMode::CleanDirectory { input_dir } => {
            let input_dir = input_dir.clone();
            feed_directory(
                ctx.clone(),
                progress.clone(),
                raw_tx,
                &input_dir,
                shutdown.clone(),
            )
            .await
        }
    };

    let mut critical: Option<ScraperError> = None;
    for outcome in [
        fetch_handle.await,
        extract_handle.await,
        filter_handle.await,
        dedup_handle.await,
        writer_handle.await,
    ] {
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                if critical.is_none() {
                    critical = Some(error);
                }
            }
            Err(error) => {
                if critical.is_none() {
                    critical = Some(error.into());
                }
            }
        }
    }

    stop_ticker.store(true, Ordering::Relaxed);
    signal_watcher.abort();
    ticker.abort();
    dashboard.finish();

    if let Err(error) = store.flush() {
        if critical.is_none() {
            critical = Some(error);
        }
    }

    if let Some(error) = feed_result.err() {
        tracing::error!(%error, "input feed failed");
        if critical.is_none() {
            critical = Some(error);
        }
    }
    if let Some(error) = critical {
        return Err(error);
    }

    Ok(RunSummary {
        snapshot: stats.snapshot(),
        manifest_path: config.general.output_dir.join(&config.output.manifest_name),
    })
}

// ---------------------------------------------------------------------------
// Feeder: persistent frontier (crawl) or local files (clean-html)
// ---------------------------------------------------------------------------

async fn feed_crawl(
    ctx: StageContext,
    progress: Arc<Progress>,
    jobs_tx: mpsc::Sender<FetchJob>,
    shutdown: Arc<AtomicBool>,
    saved_urls: Option<Arc<Mutex<SavedUrls>>>,
) -> Result<()> {
    let max_pages = ctx.config.frontier.max_pages;
    let mut dispatched: u64 = 0;
    let mut since_stats: u64 = 0;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            tracing::info!("shutdown requested, stopping frontier feed");
            break;
        }
        if max_pages > 0 && dispatched >= max_pages {
            tracing::info!(max_pages, "page budget reached");
            break;
        }
        let store = ctx.store.clone();
        let next = tokio::task::spawn_blocking(move || store.pop_next()).await??;
        match next {
            Some(entry) => {
                // A fresh database would fetch every URL again; the saved-page
                // file makes an already-written content page a pre-fetch skip.
                let already_saved = saved_urls.as_ref().is_some_and(|saved| {
                    saved
                        .lock()
                        .map(|saved| saved.contains(&entry.url))
                        .unwrap_or(false)
                });
                if already_saved {
                    tracing::trace!(url = %entry.url, "content page already saved; skipping");
                    continue;
                }
                dispatched += 1;
                since_stats += 1;
                progress.dispatch();
                if since_stats >= 32 {
                    since_stats = 0;
                    ctx.stats.set_queue_depth(ctx.store.pending_count());
                    ctx.stats.set_visited(ctx.store.visited_count());
                }
                let job = FetchJob {
                    entry,
                    _guard: CompletionGuard::new(progress.clone()),
                };
                if jobs_tx.send(job).await.is_err() {
                    break;
                }
            }
            None => {
                if progress.dispatched() == progress.completed() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    drop(jobs_tx);
    ctx.stats.set_queue_depth(ctx.store.pending_count());
    ctx.stats.set_visited(ctx.store.visited_count());
    ctx.store.flush()?;
    Ok(())
}

async fn feed_directory(
    ctx: StageContext,
    progress: Arc<Progress>,
    raw_tx: mpsc::Sender<RawPage>,
    input_dir: &Path,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    if !input_dir.is_dir() {
        return Err(ScraperError::config(format!(
            "input directory does not exist: {}",
            input_dir.display()
        )));
    }
    let files = collect_input_files(input_dir)?;
    if files.is_empty() {
        return Err(ScraperError::config(format!(
            "no .html/.htm/.xhtml/.jsonl/.jsonl.zst files under {}",
            input_dir.display()
        )));
    }
    tracing::info!(files = files.len(), "cleaning local files");

    for path in files {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let result = if is_jsonl(&path) {
            feed_jsonl(&ctx, &progress, &raw_tx, &path, &shutdown).await
        } else {
            feed_html_file(&ctx, &progress, &raw_tx, &path).await
        };
        if let Err(error) = result {
            tracing::warn!(path = %path.display(), %error, "skipping input file");
        }
    }
    Ok(())
}

async fn feed_html_file(
    ctx: &StageContext,
    progress: &Arc<Progress>,
    raw_tx: &mpsc::Sender<RawPage>,
    path: &Path,
) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|e| ScraperError::io_at(path, e))?;
    let limit = ctx.config.fetch.max_response_bytes * 8;
    if metadata.len() > limit {
        tracing::warn!(path = %path.display(), size = metadata.len(), "input file exceeds the size budget");
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(|e| ScraperError::io_at(path, e))?;
    let html = crate::fetcher::http::decode_body(&bytes, "text/html");
    let url = Url::from_file_path(path)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| format!("file://{}", path.display()));
    progress.dispatch();
    raw_tx
        .send(RawPage {
            url,
            domain: "local".to_string(),
            html,
            depth: 0,
            scraped_at: Utc::now(),
            _guard: CompletionGuard::new(progress.clone()),
        })
        .await
        .map_err(|_| ScraperError::Other("extraction stage closed".to_string()))
}

async fn feed_jsonl(
    _ctx: &StageContext,
    progress: &Arc<Progress>,
    raw_tx: &mpsc::Sender<RawPage>,
    path: &Path,
    shutdown: &Arc<AtomicBool>,
) -> Result<()> {
    use std::io::BufRead;

    let reader: Box<dyn BufRead> = if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("zst"))
        .unwrap_or(false)
    {
        let file = std::fs::File::open(path).map_err(|e| ScraperError::io_at(path, e))?;
        let decoder = zstd::stream::read::Decoder::new(std::io::BufReader::new(file))
            .map_err(|error| ScraperError::Zstd(format!("{}: {error}", path.display())))?;
        Box::new(std::io::BufReader::new(decoder))
    } else {
        let file = std::fs::File::open(path).map_err(|e| ScraperError::io_at(path, e))?;
        Box::new(std::io::BufReader::new(file))
    };

    for (line_number, line) in reader.lines().enumerate() {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let line = line.map_err(|e| ScraperError::io_at(path, e))?;
        if line.trim().is_empty() {
            continue;
        }
        let record: InputRecord = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(error) => {
                tracing::warn!(path = %path.display(), line = line_number + 1, %error, "invalid jsonl input record");
                continue;
            }
        };
        let url = record
            .url
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| format!("file://{}#{}", path.display(), line_number + 1));
        let domain = Url::parse(&url)
            .ok()
            .and_then(|parsed| registrable_domain(&parsed))
            .unwrap_or_else(|| "local".to_string());
        progress.dispatch();
        raw_tx
            .send(RawPage {
                url,
                domain,
                html: record.html,
                depth: 0,
                scraped_at: Utc::now(),
                _guard: CompletionGuard::new(progress.clone()),
            })
            .await
            .map_err(|_| ScraperError::Other("extraction stage closed".to_string()))?;
    }
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct InputRecord {
    #[serde(default)]
    url: Option<String>,
    #[serde(alias = "body", alias = "content", alias = "html_body")]
    html: String,
}

fn is_jsonl(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")
}

fn collect_input_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).map_err(|e| ScraperError::io_at(&current, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| ScraperError::io_at(&current, e))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| ScraperError::io_at(&path, e))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && is_supported_input(&path) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn is_supported_input(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".html")
        || name.ends_with(".htm")
        || name.ends_with(".xhtml")
        || name.ends_with(".jsonl")
        || name.ends_with(".jsonl.zst")
}

// ---------------------------------------------------------------------------
// Seed handling (including robots.txt sitemap discovery)
// ---------------------------------------------------------------------------

/// True when `url` is already listed in the saved content-page file.
fn is_saved(saved_urls: &Option<Arc<Mutex<SavedUrls>>>, url: &str) -> bool {
    saved_urls.as_ref().is_some_and(|saved| {
        saved
            .lock()
            .map(|saved| saved.contains(url))
            .unwrap_or(false)
    })
}

async fn seed_frontier(
    ctx: &StageContext,
    robots: &Arc<RobotsCache>,
    fetcher: &Fetcher,
    seeds: &[String],
    saved_urls: &Option<Arc<Mutex<SavedUrls>>>,
) -> Result<usize> {
    let mut inserted = 0usize;
    let mut accepted: Vec<Url> = Vec::new();

    for raw in seeds {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parsed = match Url::parse(trimmed) {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(seed = trimmed, %error, "invalid seed url");
                continue;
            }
        };
        let Some(url) = crate::frontier::normalize_absolute(&parsed) else {
            tracing::warn!(seed = trimmed, "seed url is not crawlable");
            continue;
        };
        if !domain_permitted(
            &url,
            &ctx.config.frontier.allowed_domains,
            &ctx.config.frontier.blocked_domains,
        ) {
            tracing::warn!(seed = %url, "seed blocked by domain filters");
            continue;
        }
        let Some(domain) = registrable_domain(&url) else {
            continue;
        };
        let entry = UrlEntry {
            url: url.to_string(),
            domain,
            depth: 0,
            priority: ctx.config.frontier.seed_priority,
        };
        if is_saved(saved_urls, &entry.url) {
            continue;
        }
        if matches!(ctx.store.enqueue(&entry)?, EnqueueOutcome::Inserted) {
            inserted += 1;
            accepted.push(url);
        }
    }

    if !ctx.config.frontier.ignore_robots && ctx.config.frontier.discover_sitemaps {
        let client = fetcher.client();
        let semaphore = Arc::new(Semaphore::new(12));
        let mut tasks: JoinSet<usize> = JoinSet::new();
        for url in accepted {
            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            let ctx = (*ctx).clone();
            let client = client.clone();
            let robots = Arc::clone(robots);
            let saved_urls = saved_urls.clone();
            tasks.spawn(async move {
                let mut added = 0usize;
                let rules = robots.rules_for_url(&url).await;
                for sitemap in rules.sitemaps().iter().take(3) {
                    added += ingest_sitemap(&ctx, &client, sitemap, &saved_urls).await;
                }
                drop(permit);
                added
            });
        }
        while let Some(result) = tasks.join_next().await {
            inserted += result.unwrap_or(0);
        }
    }
    Ok(inserted)
}

async fn ingest_sitemap(
    ctx: &StageContext,
    client: &reqwest::Client,
    sitemap: &str,
    saved_urls: &Option<Arc<Mutex<SavedUrls>>>,
) -> usize {
    let Ok(url) = Url::parse(sitemap) else {
        return 0;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return 0;
    }
    let response = match client.get(url.clone()).send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(%sitemap, %error, "sitemap fetch failed");
            return 0;
        }
    };
    if !response.status().is_success() {
        return 0;
    }
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::debug!(%sitemap, %error, "sitemap body failed");
            return 0;
        }
    };
    let limit = bytes.len().min(8 * 1024 * 1024);
    let text = String::from_utf8_lossy(&bytes[..limit]);
    let mut inserted = 0usize;
    for captures in SITEMAP_LOC.captures_iter(&text) {
        let Some(location) = captures.get(1) else {
            continue;
        };
        let Ok(candidate) = Url::parse(location.as_str().trim()) else {
            continue;
        };
        let Some(normalized) = crate::frontier::normalize_absolute(&candidate) else {
            continue;
        };
        if !domain_permitted(
            &normalized,
            &ctx.config.frontier.allowed_domains,
            &ctx.config.frontier.blocked_domains,
        ) {
            continue;
        }
        let Some(domain) = registrable_domain(&normalized) else {
            continue;
        };
        let entry = UrlEntry {
            url: normalized.to_string(),
            domain,
            depth: 0,
            priority: ctx.config.frontier.seed_priority.saturating_sub(10),
        };
        if is_saved(saved_urls, &entry.url) {
            continue;
        }
        if matches!(ctx.store.enqueue(&entry), Ok(EnqueueOutcome::Inserted)) {
            inserted += 1;
        }
    }
    if inserted > 0 {
        tracing::info!(sitemap, inserted, "discovered urls from sitemap");
    }
    inserted
}

// ---------------------------------------------------------------------------
// Stage implementations
// ---------------------------------------------------------------------------

async fn cpu_stage<I, O, F>(
    mut rx: mpsc::Receiver<I>,
    out_tx: mpsc::Sender<O>,
    workers: usize,
    work: F,
) -> Result<()>
where
    I: Send + 'static,
    O: Send + 'static,
    F: Fn(I) -> Option<O> + Send + Sync + 'static,
{
    let semaphore = Arc::new(Semaphore::new(workers.max(1)));
    let work = Arc::new(work);
    let mut tasks: JoinSet<()> = JoinSet::new();

    while let Some(item) = rx.recv().await {
        let Ok(permit) = semaphore.clone().acquire_owned().await else {
            return Err(ScraperError::Other("stage semaphore closed".to_string()));
        };
        let out = out_tx.clone();
        let work = work.clone();
        tasks.spawn(async move {
            match tokio::task::spawn_blocking(move || work(item)).await {
                Ok(Some(output)) => {
                    let _ = out.send(output).await;
                }
                Ok(None) => {}
                Err(error) => tracing::error!(%error, "blocking stage task panicked"),
            }
            drop(permit);
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn fetch_stage(
    mut rx: mpsc::Receiver<FetchJob>,
    raw_tx: mpsc::Sender<RawPage>,
    ctx: StageContext,
    fetcher: Fetcher,
    scheduler: Arc<DomainScheduler>,
    robots: Arc<RobotsCache>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let workers = ctx.config.pipeline.resolved_fetch_workers().max(1);
    let semaphore = Arc::new(Semaphore::new(workers));
    let mut tasks: JoinSet<()> = JoinSet::new();

    while let Some(job) = rx.recv().await {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let Ok(permit) = semaphore.clone().acquire_owned().await else {
            break;
        };
        let raw_tx = raw_tx.clone();
        let ctx = ctx.clone();
        let fetcher = fetcher.clone();
        let scheduler = scheduler.clone();
        let robots = robots.clone();
        tasks.spawn(async move {
            let outcome = fetch_one(&ctx, &fetcher, &scheduler, &robots, &job.entry).await;
            match outcome {
                FetchOutcome::Page(page) => {
                    tracing::debug!(
                        url = %page.url,
                        domain = %job.entry.domain,
                        depth = job.entry.depth,
                        "fetched page"
                    );
                    let raw = RawPage {
                        url: page.url.to_string(),
                        domain: job.entry.domain.clone(),
                        html: page.html,
                        depth: job.entry.depth,
                        scraped_at: Utc::now(),
                        _guard: job._guard,
                    };
                    let _ = raw_tx.send(raw).await;
                }
                FetchOutcome::Skipped(reason) => {
                    ctx.stats.record_reject(reason);
                    tracing::debug!(url = %job.entry.url, %reason, "url skipped");
                }
            }
            drop(permit);
        });
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

async fn fetch_one(
    ctx: &StageContext,
    fetcher: &Fetcher,
    scheduler: &DomainScheduler,
    robots: &RobotsCache,
    entry: &UrlEntry,
) -> FetchOutcome {
    let Ok(url) = Url::parse(&entry.url) else {
        return FetchOutcome::Skipped(RejectReason::FetchFailed);
    };

    let mut crawl_delay = None;
    if !ctx.config.frontier.ignore_robots {
        let rules = robots.rules_for_url(&url).await;
        let mut target = url.path().to_string();
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        if !rules.allowed(&target) {
            ctx.stats.record_robots_blocked();
            tracing::debug!(url = %url, "disallowed by robots.txt");
            return FetchOutcome::Skipped(RejectReason::RobotsBlocked);
        }
        if ctx.config.frontier.respect_crawl_delay {
            crawl_delay = rules.crawl_delay();
        }
    }

    let permit = match scheduler.acquire(&entry.domain, crawl_delay).await {
        Ok(permit) => permit,
        Err(error) => {
            tracing::warn!(domain = %entry.domain, %error, "scheduler failure");
            return FetchOutcome::Skipped(RejectReason::FetchFailed);
        }
    };
    let result = fetcher.fetch(&url).await;
    drop(permit);

    match result {
        Ok(page) => {
            ctx.stats.record_fetch_ok();
            FetchOutcome::Page(page)
        }
        Err(error) => {
            ctx.stats.record_fetch_error();
            tracing::debug!(url = %url, %error, "fetch failed");
            let reason = match error.kind {
                FetchFailureKind::BodyTooLarge => RejectReason::ResponseTooLarge,
                FetchFailureKind::UnsupportedContentType => RejectReason::NonHtmlResponse,
                _ => RejectReason::FetchFailed,
            };
            FetchOutcome::Skipped(reason)
        }
    }
}

fn extract_work(ctx: &StageContext, page: RawPage) -> Option<ExtractedItem> {
    let base = match Url::parse(&page.url) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(url = %page.url, %error, "cannot parse page url");
            return None;
        }
    };
    let extracted = extractor::extract(&page.html, &base, &ctx.config.extract);
    // Clean before the quality filter and before anything is stored, so the
    // saved corpus (txt and jsonl alike) never contains bibliography dumps,
    // bare links or navigation labels that only the renderer used to remove.
    let text = sanitize_markdown(&extracted.text);
    if text.trim().is_empty() {
        ctx.stats.record_reject(RejectReason::EmptyContent);
        return None;
    }
    ctx.stats.record_extracted();
    if ctx.crawl && page.depth < ctx.config.frontier.max_depth {
        enqueue_links(ctx, &extracted.links, page.depth + 1);
    }
    Some(ExtractedItem {
        url: page.url,
        domain: page.domain,
        title: clean_title(&extracted.title),
        text,
        scraped_at: page.scraped_at,
        _guard: page._guard,
    })
}

fn enqueue_links(ctx: &StageContext, links: &[String], depth: u32) {
    let priority = priority_for_depth(depth);
    let mut inserted = 0usize;
    for link in links {
        let Ok(url) = Url::parse(link) else {
            continue;
        };
        if !domain_permitted(
            &url,
            &ctx.config.frontier.allowed_domains,
            &ctx.config.frontier.blocked_domains,
        ) {
            continue;
        }
        let Some(domain) = registrable_domain(&url) else {
            continue;
        };
        let entry = UrlEntry {
            url: link.clone(),
            domain,
            depth,
            priority,
        };
        match ctx.store.enqueue(&entry) {
            Ok(EnqueueOutcome::Inserted) => inserted += 1,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "cannot enqueue discovered link");
                break;
            }
        }
    }
    if inserted > 0 {
        tracing::trace!(inserted, depth, "discovered links");
    }
}

fn filter_work(ctx: &StageContext, item: ExtractedItem) -> Option<CleanItem> {
    if item.text.trim().is_empty() {
        ctx.stats.record_reject(RejectReason::EmptyContent);
        return None;
    }
    if item.text.chars().count() < ctx.config.extract.min_block_chars {
        ctx.stats.record_reject(RejectReason::TooShortContent);
        return None;
    }
    match filter::evaluate(&item.text, &ctx.config.filter) {
        Ok(clean) => Some(CleanItem {
            url: item.url,
            domain: item.domain,
            title: item.title,
            clean,
            scraped_at: item.scraped_at,
            _guard: item._guard,
        }),
        Err(reason) => {
            ctx.stats.record_reject(reason);
            tracing::trace!(url = %item.url, %reason, "document rejected by filters");
            None
        }
    }
}

fn dedup_work(ctx: &StageContext, engine: &DedupEngine, item: CleanItem) -> Option<AcceptedItem> {
    let processed = match engine.process(&item.clean.text) {
        Ok(processed) => processed,
        Err(error) => {
            tracing::warn!(url = %item.url, %error, "deduplication failed");
            return None;
        }
    };
    if let Some(reason) = processed.duplicate {
        ctx.stats.record_reject(reason);
        return None;
    }

    let text = processed.text;
    let word_count = text.split_whitespace().count();
    let char_count = text.chars().count();
    let document = Document {
        id: blake3::hash(text.as_bytes()).to_hex().to_string(),
        url: item.url,
        domain: item.domain,
        title: item.title,
        language: item.clean.language,
        language_score: (item.clean.language_score * 10_000.0).round() / 10_000.0,
        word_count,
        char_count,
        simhash: format!("0x{:016x}", processed.simhash),
        scraped_at: item.scraped_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        text,
    };
    Some(AcceptedItem {
        doc: document,
        _guard: item._guard,
    })
}

async fn writer_stage(
    mut rx: mpsc::Receiver<AcceptedItem>,
    config: Arc<Config>,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    saved_urls: Option<Arc<Mutex<SavedUrls>>>,
) -> Result<()> {
    let mut writer = ShardWriter::new(&config.general.output_dir, &config.output, stats)?;
    let mut fatal: Option<ScraperError> = None;
    while let Some(item) = rx.recv().await {
        if fatal.is_some() {
            continue;
        }
        match writer.write(&item.doc) {
            Ok(true) => {
                // Only pages that actually produced records are remembered, so
                // an empty or fully duplicate page is retried on the next run.
                if let Some(saved) = &saved_urls {
                    if let Ok(mut saved) = saved.lock() {
                        match saved.record(&item.doc.url) {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::trace!(url = %item.doc.url, "page is not a content page; url not logged")
                            }
                            Err(error) => {
                                tracing::warn!(url = %item.doc.url, %error, "cannot append saved page url")
                            }
                        }
                    }
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "shard write failed; stopping the crawl");
                fatal = Some(error);
                shutdown.store(true, Ordering::Relaxed);
            }
        }
    }
    if let Some(saved) = &saved_urls {
        if let Ok(mut saved) = saved.lock() {
            if let Err(error) = saved.flush() {
                tracing::warn!(%error, "cannot flush saved page urls");
            }
        }
    }
    if let Err(error) = writer.finalize() {
        if fatal.is_none() {
            fatal = Some(error);
        }
    }
    match fatal {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

fn spawn_dashboard(
    dashboard: Arc<Dashboard>,
    stats: Arc<Stats>,
    progress: Arc<Progress>,
    stop: Arc<AtomicBool>,
    interval_ms: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_millis(interval_ms.max(50));
        while !stop.load(Ordering::Relaxed) {
            dashboard.render(&stats, &progress);
            tokio::time::sleep(interval).await;
        }
    })
}

fn spawn_signal_watcher(shutdown: Arc<AtomicBool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            match terminate.as_mut() {
                Some(signal) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {},
                        _ = signal.recv() => {},
                    }
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::warn!("shutdown signal received: finishing in-flight work and closing shards");
        shutdown.store(true, Ordering::SeqCst);
    })
}
