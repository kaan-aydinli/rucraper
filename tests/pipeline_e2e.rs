//! End-to-end tests for the bounded pipeline.
//!
//! A tiny in-process HTTP server (plain `tokio::net::TcpListener`, no test
//! framework) serves a robots.txt plus a small linked site. The crawl runs with
//! a 1 KiB shard limit so that shard rotation, Zstd framing, the JSON schema and
//! the manifest SHA-256 values are all exercised by one run.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pretrain_scraper::config::{
    Config, DedupConfig, ExtractConfig, FetchConfig, FilterConfig, FrontierConfig, GeneralConfig,
    OutputConfig, PipelineConfig,
};
use pretrain_scraper::pipeline::{self, RunMode};
use pretrain_scraper::storage::{load_manifest, verify_dataset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const RUST_TEXT: &str = "Rust is a systems programming language focused on safety, speed and concurrency. \
Ownership and borrowing are checked at compile time, so data races are rejected before the program runs. \
The compiler emits precise diagnostics that point at the exact expression which broke a rule. \
Cargo resolves dependencies from a lockfile and keeps builds reproducible across machines. \
Async runtimes schedule thousands of lightweight tasks on a small thread pool. \
Memory is released deterministically when values leave scope, without a garbage collector. \
Tooling integrates with editors to provide instant feedback while typing. \
Teams report fewer production incidents after migrating latency sensitive services.";

const OCEAN_TEXT: &str = "Marine biologists study organisms that live in salt water, from plankton to whales. \
Coral reefs support roughly a quarter of all marine species while covering a tiny fraction of the sea floor. \
Researchers tag migratory fish to map their routes across entire ocean basins. \
Conservation teams restore damaged habitats and monitor water quality over decades. \
Deep sea vents host ecosystems that never receive sunlight and rely on chemical energy instead. \
Fisheries managers use population models to set quotas that keep stocks healthy. \
Satellite imagery reveals algal blooms that threaten coastal economies every summer. \
Public aquariums fund field research and educate visitors about fragile ecosystems.";

const HISTORY_TEXT: &str = "Medieval towns grew around markets, rivers and defensive walls. \
Guilds regulated craft production and trained apprentices over many years. \
Scribes copied manuscripts by hand, which made books expensive and rare. \
Trade routes connected the Baltic to the Mediterranean through a network of fairs. \
Cathedrals took generations to build and employed hundreds of skilled workers. \
Local courts recorded disputes about land, inheritance and commercial contracts. \
Plague outbreaks reshaped labour markets by reducing the available workforce. \
Historians reconstruct daily life from account books, wills and tax registers.";

const COOKING_TEXT: &str =
    "A good bread dough needs flour, water, salt and time rather than complicated equipment. \
Mixing develops gluten, which gives the loaf structure and a pleasant chew. \
Long fermentation builds flavour and helps the dough digest more easily. \
Bakers shape the loaf gently to keep the gas that the yeast produced. \
A hot oven with steam produces a crisp crust and an open crumb. \
Cooling the bread on a rack prevents the base from becoming soggy. \
Whole grain flours absorb more water and benefit from a slower proof. \
Keeping a starter alive requires regular feeding and a little patience.";

/// Serve `/robots.txt` plus a four page site, one request per connection.
async fn spawn_site() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, content_type, body) = match path.as_str() {
                    "/robots.txt" => (
                        "200 OK",
                        "text/plain",
                        "User-agent: *\nAllow: /\nDisallow: /private/\n".to_string(),
                    ),
                    "/" => ("200 OK", "text/html; charset=utf-8", index_page()),
                    "/rust" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        article_page("Rust for systems work", "rust", RUST_TEXT),
                    ),
                    "/ocean" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        article_page("Life in the ocean", "ocean", OCEAN_TEXT),
                    ),
                    "/history" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        article_page("Medieval town life", "history", HISTORY_TEXT),
                    ),
                    "/cooking" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        article_page("Baking sourdough", "cooking", COOKING_TEXT),
                    ),
                    "/private/secret" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        article_page("Secret", "secret", RUST_TEXT),
                    ),
                    _ => ("404 Not Found", "text/plain", "not found".to_string()),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (address, handle)
}

fn article_page(title: &str, slug: &str, text: &str) -> String {
    let paragraphs: String = text
        .split_inclusive(". ")
        .map(|sentence| format!("<p>{}</p>", sentence.trim()))
        .collect();
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>{title}</title>
<style>.x{{color:red}}</style><script>var tracking = 1;</script></head>
<body>
<div class="cookie-banner">We use cookies to improve your experience.</div>
<header class="site-header"><nav><a href="/">Home</a><a href="/rust">Rust</a></nav></header>
<article id="post-{slug}">
<h1>{title}</h1>
{paragraphs}
</article>
<aside class="sidebar"><h3>Popular</h3><a href="/ocean">Ocean</a></aside>
<footer class="site-footer">Copyright Example</footer>
</body></html>"#
    )
}

fn index_page() -> String {
    String::from(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Corpus index</title></head>
<body>
<nav class="menu"><a href="/rust">Rust</a><a href="/ocean">Ocean</a>
<a href="/history">History</a><a href="/cooking">Cooking</a>
<a href="/private/secret">Secret</a></nav>
<main><article>
<h1>Corpus index</h1>
<p>A pretraining corpus collects clean prose from many sources and normalizes it.</p>
<p>Quality filters remove navigation, advertisements and machine generated spam.</p>
<p>Deduplication keeps a single copy of each document and of near identical mirrors.</p>
<p>Each record stores its source URL, language, word counts and a fingerprint.</p>
<p>Shards are compressed independently so a failing file loses only a small slice.</p>
<p>A manifest records sizes and digests for integrity verification later.</p>
<p>Operators can resume a crawl because the frontier lives in an embedded database.</p>
<p>Backpressure keeps memory flat even when the crawl runs for several days.</p>
</article></main>
</body></html>"#,
    )
}

fn test_config(root: &Path) -> Config {
    Config {
        general: GeneralConfig {
            output_dir: root.join("dataset"),
            db_path: root.join("db"),
            log_level: "warn".to_string(),
            json_logs: false,
            progress: false,
        },
        frontier: FrontierConfig {
            max_depth: 2,
            min_delay_ms: 0,
            max_concurrency_per_domain: 4,
            robots_ttl_secs: 60,
            queue_capacity: 1_000,
            max_pages: 20,
            ..FrontierConfig::default()
        },
        fetch: FetchConfig {
            max_retries: 1,
            retry_base_delay_ms: 10,
            request_timeout_secs: 5,
            ..FetchConfig::default()
        },
        extract: ExtractConfig {
            min_block_chars: 40,
            ..ExtractConfig::default()
        },
        filter: FilterConfig {
            min_words: 40,
            min_lang_confidence: 0.3,
            target_languages: vec!["en".into()],
            min_punctuation_line_ratio: 0.5,
            ..FilterConfig::default()
        },
        dedup: DedupConfig {
            max_hamming_distance: 3,
            ..DedupConfig::default()
        },
        output: OutputConfig {
            shard_size_bytes: 1_024,
            zstd_level: 3,
            ..OutputConfig::default()
        },
        pipeline: PipelineConfig {
            channel_capacity: 8,
            fetch_workers: 4,
            extract_workers: 2,
            filter_workers: 2,
            dedup_workers: 2,
            ..PipelineConfig::default()
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawl_end_to_end_rotates_shards_and_verifies() {
    let (address, server) = spawn_site().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(dir.path());
    let output_dir = config.general.output_dir.clone();
    let manifest_name = config.output.manifest_name.clone();
    let seed = format!("http://{address}/");

    let summary = tokio::time::timeout(
        Duration::from_secs(120),
        pipeline::run(config, RunMode::Crawl { seeds: vec![seed] }),
    )
    .await
    .expect("crawl must terminate")
    .expect("crawl succeeds");

    server.abort();

    assert_eq!(summary.snapshot.fetched, 5, "four pages plus the index");
    assert!(
        summary.snapshot.fetch_errors == 0,
        "no fetch errors expected"
    );
    assert_eq!(
        summary.snapshot.robots_blocked, 1,
        "the linked /private/secret page must be blocked by robots.txt"
    );
    assert!(
        summary.snapshot.accepted_docs >= 4,
        "expected at least four accepted documents, got {}",
        summary.snapshot.accepted_docs
    );
    assert!(
        summary.snapshot.shards >= 2,
        "a 1 KiB shard limit must rotate, got {} shard(s)",
        summary.snapshot.shards
    );

    let manifest = load_manifest(&output_dir, &manifest_name)
        .expect("manifest readable")
        .expect("manifest exists");
    assert_eq!(manifest.shards.len() as u64, summary.snapshot.shards);
    assert_eq!(manifest.documents, summary.snapshot.accepted_docs);
    assert!(manifest.words > 0);
    assert!(manifest.shards.iter().all(|shard| shard.sha256.len() == 64));
    assert!(
        manifest.shards.iter().all(|shard| shard.size_bytes > 0),
        "every shard must have bytes on disk"
    );

    let report = verify_dataset(&output_dir, &manifest_name).expect("verification runs");
    assert!(
        report.is_ok(),
        "verification failures: {:?}",
        report.failures
    );
    assert_eq!(report.documents, manifest.documents);

    // The frontier must be drained and the visited set must contain every URL
    // that was discovered, including the disallowed one (filtered by robots).
    let summary_stats = pretrain_scraper::frontier::Store::open(&dir.path().join("db"), 1_000)
        .expect("reopen store")
        .pending_count();
    assert_eq!(summary_stats, 0, "frontier must be empty after the crawl");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verify_detects_corrupted_shard() {
    let (address, server) = spawn_site().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(dir.path());
    let output_dir = config.general.output_dir.clone();
    let manifest_name = config.output.manifest_name.clone();

    pipeline::run(
        config,
        RunMode::Crawl {
            seeds: vec![format!("http://{address}/")],
        },
    )
    .await
    .expect("crawl succeeds");
    server.abort();

    let manifest = load_manifest(&output_dir, &manifest_name)
        .expect("manifest")
        .expect("exists");
    let first = &manifest.shards[0];
    let path = output_dir.join(&first.name);
    let mut bytes = std::fs::read(&path).expect("read shard");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).expect("corrupt shard");

    let report = verify_dataset(&output_dir, &manifest_name).expect("verification runs");
    assert!(!report.is_ok(), "corruption must be detected");
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.contains("sha256")),
        "expected a sha256 failure, got {:?}",
        report.failures
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_directory_pipeline_writes_dataset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input_dir = dir.path().join("raw_html");
    std::fs::create_dir_all(&input_dir).expect("input dir");
    std::fs::write(
        input_dir.join("rust.html"),
        article_page("Rust systems", "rust", RUST_TEXT),
    )
    .expect("write html");
    std::fs::write(
        input_dir.join("ocean.html"),
        article_page("Ocean life", "ocean", OCEAN_TEXT),
    )
    .expect("write html");
    // A bibliography line and a navigation label inside the article body: the
    // crawler must remove them before the record is written, not at render time.
    let history_html = article_page("Town life", "history", HISTORY_TEXT).replace(
        "</article>",
        "<p>↑ Kaynak. Erişim tarihi: 3 Mayıs 2019.</p><p>Ana Sayfa</p></article>",
    );
    let jsonl = serde_json::json!({
        "url": "https://example.org/history",
        "html": history_html,
    });
    std::fs::write(input_dir.join("records.jsonl"), format!("{jsonl}\n")).expect("write jsonl");

    let config = test_config(dir.path());
    let output_dir = config.general.output_dir.clone();
    let manifest_name = config.output.manifest_name.clone();

    let summary = pipeline::run(
        config,
        RunMode::CleanDirectory {
            input_dir: input_dir.clone(),
        },
    )
    .await
    .expect("clean-html succeeds");

    assert_eq!(summary.snapshot.fetched, 0, "no network access expected");
    assert_eq!(summary.snapshot.accepted_docs, 3, "three local documents");
    let manifest = load_manifest(&output_dir, &manifest_name)
        .expect("manifest")
        .expect("exists");
    assert_eq!(manifest.documents, 3);
    let report = verify_dataset(&output_dir, &manifest_name).expect("verification runs");
    assert!(report.is_ok(), "failures: {:?}", report.failures);

    let mut corpus = String::new();
    for entry in std::fs::read_dir(&output_dir).expect("dataset dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("txt") {
            corpus.push_str(&std::fs::read_to_string(&path).expect("read corpus"));
        }
    }
    assert!(
        !corpus.contains("Erişim tarihi"),
        "bibliography line was stored: {corpus}"
    );
    assert!(
        !corpus.contains("Ana Sayfa"),
        "navigation label was stored: {corpus}"
    );

    // Re-running must not duplicate anything: exact dedup rejects all three.
    let summary_again = pipeline::run(
        Config::load(&write_config(dir.path(), &input_dir, &output_dir)).expect("config loads"),
        RunMode::CleanDirectory { input_dir },
    )
    .await
    .expect("second run succeeds");
    assert_eq!(
        summary_again.snapshot.accepted_docs, 0,
        "rerun must be fully deduplicated"
    );
}

#[tokio::test]
async fn saved_content_pages_are_skipped_on_a_fresh_crawl() {
    let (address, server) = spawn_site().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(dir.path());
    let output_dir = config.general.output_dir.clone();
    std::fs::create_dir_all(&output_dir).expect("dataset dir");

    // Pretend an earlier run stored the ocean article; a fresh database must
    // still skip it before any request is made.
    let saved_url = format!("http://{address}/ocean");
    std::fs::write(
        output_dir.join(&config.output.saved_urls_file),
        format!("{saved_url}\n"),
    )
    .expect("write saved urls");

    let seed = format!("http://{address}/");
    let summary = tokio::time::timeout(
        Duration::from_secs(120),
        pipeline::run(config, RunMode::Crawl { seeds: vec![seed] }),
    )
    .await
    .expect("crawl must terminate")
    .expect("crawl succeeds");
    server.abort();

    assert_eq!(
        summary.snapshot.fetched, 4,
        "the saved article must not be fetched (index plus three others)"
    );

    let mut corpus = String::new();
    for entry in std::fs::read_dir(&output_dir).expect("dataset dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("txt")
            && path.file_name().and_then(|name| name.to_str()) != Some("crawled_urls.txt")
        {
            corpus.push_str(&std::fs::read_to_string(&path).expect("read corpus"));
        }
    }
    assert!(
        !corpus.contains("Marine biologists study organisms"),
        "skipped page leaked into the corpus: {corpus}"
    );
    assert!(corpus.contains("Rust is a systems programming language"));
}

/// Persist a config file so the rerun resumes the same database and dataset.
fn write_config(root: &Path, input_dir: &Path, output_dir: &Path) -> std::path::PathBuf {
    let mut config = test_config(root);
    config.general.output_dir = output_dir.to_path_buf();
    let _ = input_dir;
    let toml = toml::to_string(&config).expect("serialize config");
    let path = root.join("rerun.toml");
    std::fs::write(&path, toml).expect("write config");
    path
}

#[test]
fn pipeline_config_is_shared_for_tests() {
    // Guards against accidental changes to the test tuning above.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(dir.path());
    let shared: Arc<Config> = Arc::new(config);
    assert_eq!(shared.output.shard_size_bytes, 1_024);
    assert!(shared.filter.min_words < shared.filter.max_words);
}
