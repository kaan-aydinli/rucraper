//! Command line interface for the crawler and dataset cleaner.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use pretrain_scraper::config::Config;
use pretrain_scraper::export::{
    export_txt, ExportOptions, DEFAULT_DEDUP_INDEX_CAPACITY, DEFAULT_DEDUP_MIN_SENTENCE_WORDS,
    DEFAULT_MAX_CHUNK_WORDS, DEFAULT_MIN_CHUNK_WORDS, DEFAULT_SEPARATOR,
};
use pretrain_scraper::frontier::Store;
use pretrain_scraper::pipeline::{self, RunMode};
use pretrain_scraper::storage::{self, verify_dataset};
use pretrain_scraper::telemetry::StatsSnapshot;

#[derive(Debug, Parser)]
#[command(
    name = "pretrain-scraper",
    version,
    about = "Bounded-memory web crawler and cleaner that emits LLM pretraining shards",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Crawl from seeds (resumes from the persistent frontier when restarted).
    Crawl(CrawlArgs),
    /// Clean local HTML/JSONL files without any network access.
    CleanHtml(CleanHtmlArgs),
    /// Report frontier and dataset statistics.
    Stats(StatsArgs),
    /// Verify shard SHA-256 hashes and the JSON schema of a dataset.
    Verify(VerifyArgs),
    /// Export a dataset as one plain-text pretraining corpus.
    ExportTxt(ExportTxtArgs),
}

#[derive(Debug, Args)]
struct CrawlArgs {
    /// Path to the TOML configuration.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// Seed URLs, one per line (blank lines and `#` comments are ignored).
    #[arg(long)]
    seeds: Option<PathBuf>,
    /// Override `general.output_dir`.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Override `general.db_path`.
    #[arg(long)]
    db_path: Option<PathBuf>,
    /// Override `frontier.max_pages` (0 = unlimited).
    #[arg(long)]
    max_pages: Option<u64>,
}

#[derive(Debug, Args)]
struct CleanHtmlArgs {
    /// Directory containing .html/.htm/.xhtml/.jsonl/.jsonl.zst inputs.
    #[arg(long)]
    input_dir: PathBuf,
    /// Directory receiving the shards and manifest.
    #[arg(long)]
    output_dir: PathBuf,
    /// Path to the TOML configuration.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// Override `general.db_path` (holds the dedup indices).
    #[arg(long)]
    db_path: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StatsArgs {
    /// Path to the TOML configuration.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// Override the dataset directory that is inspected.
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct VerifyArgs {
    /// Dataset directory containing the manifest and shards.
    #[arg(long, default_value = "./dataset")]
    output_dir: PathBuf,
    /// Manifest file name inside `output_dir`.
    #[arg(long, default_value = "manifest.json")]
    manifest: String,
}

#[derive(Debug, Args)]
struct ExportTxtArgs {
    /// Dataset directory containing the manifest and shards.
    #[arg(long, default_value = "./dataset")]
    dataset: PathBuf,
    /// Output file (defaults to `<dataset>/corpus.txt`).
    #[arg(long)]
    output: Option<PathBuf>,
    /// Manifest file name inside `dataset`.
    #[arg(long, default_value = "manifest.json")]
    manifest: String,
    /// Marker written on its own line after every document.
    #[arg(long, default_value = DEFAULT_SEPARATOR)]
    separator: String,
    /// Never write titles, even when they match the text.
    #[arg(long)]
    no_title: bool,
    /// Largest topic block, in words; longer sections are split.
    #[arg(long, default_value_t = DEFAULT_MAX_CHUNK_WORDS)]
    max_chunk_words: u64,
    /// Blocks shorter than this are merged with the following section.
    #[arg(long, default_value_t = DEFAULT_MIN_CHUNK_WORDS)]
    min_chunk_words: u64,
    /// Keep duplicate paragraphs, sentences and blocks instead of removing them.
    #[arg(long)]
    no_dedupe: bool,
    /// Shortest sentence/paragraph checked for duplication.
    #[arg(long, default_value_t = DEFAULT_DEDUP_MIN_SENTENCE_WORDS)]
    dedup_min_sentence_words: usize,
    /// Size bound of the duplicate index (constant memory).
    #[arg(long, default_value_t = DEFAULT_DEDUP_INDEX_CAPACITY)]
    dedup_index_capacity: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Crawl(args) => run_crawl(args).await,
        Command::CleanHtml(args) => run_clean_html(args).await,
        Command::Stats(args) => run_stats(args),
        Command::Verify(args) => run_verify(args),
        Command::ExportTxt(args) => run_export_txt(args),
    }
}

async fn run_crawl(args: CrawlArgs) -> Result<()> {
    let mut config = Config::load(&args.config)
        .with_context(|| format!("cannot load configuration {}", args.config.display()))?;
    if let Some(output_dir) = args.output_dir {
        config.general.output_dir = output_dir;
    }
    if let Some(db_path) = args.db_path {
        config.general.db_path = db_path;
    }
    if let Some(max_pages) = args.max_pages {
        config.frontier.max_pages = max_pages;
    }
    init_logging(&config);

    // `--seeds` wins; otherwise the site list file named in the config is both
    // the crawl scope and the seed source, so no extra flag is required.
    let seeds = match args.seeds.as_deref() {
        Some(path) => read_seeds(path)?,
        None => config.frontier.site_seeds.clone(),
    };
    let summary = pipeline::run(config, RunMode::Crawl { seeds })
        .await
        .context("crawl failed")?;
    print_summary("crawl", &summary.snapshot, &summary.manifest_path);
    Ok(())
}

async fn run_clean_html(args: CleanHtmlArgs) -> Result<()> {
    if !args.input_dir.is_dir() {
        bail!(
            "input directory does not exist: {}",
            args.input_dir.display()
        );
    }
    let mut config = Config::load(&args.config)
        .with_context(|| format!("cannot load configuration {}", args.config.display()))?;
    config.general.output_dir = args.output_dir;
    if let Some(db_path) = args.db_path {
        config.general.db_path = db_path;
    }
    init_logging(&config);

    let summary = pipeline::run(
        config,
        RunMode::CleanDirectory {
            input_dir: args.input_dir,
        },
    )
    .await
    .context("clean-html failed")?;
    print_summary("clean-html", &summary.snapshot, &summary.manifest_path);
    Ok(())
}

fn run_stats(args: StatsArgs) -> Result<()> {
    let config = Config::load(&args.config)
        .with_context(|| format!("cannot load configuration {}", args.config.display()))?;
    init_logging(&config);
    let output_dir = args
        .output_dir
        .unwrap_or_else(|| config.general.output_dir.clone());

    let store = Store::open(&config.general.db_path, config.frontier.queue_capacity).with_context(
        || {
            format!(
                "cannot open frontier at {}",
                config.general.db_path.display()
            )
        },
    )?;
    let summary = storage::dataset_summary(&output_dir, &config.output.manifest_name)?;

    println!("Frontier");
    println!("  pending URLs   : {}", count(store.pending_count()));
    println!("  visited URLs   : {}", count(store.visited_count()));
    println!(
        "  exact hashes   : {}",
        count(store.exact_tree().len() as u64)
    );
    println!(
        "  simhash bands  : {}",
        count(store.simhash_tree().len() as u64)
    );
    println!("Dataset ({})", output_dir.display());
    println!("  shards         : {}", count(summary.shards));
    println!("  documents      : {}", count(summary.documents));
    println!("  words          : {}", count(summary.words));
    println!(
        "  text bytes     : {}",
        human_bytes(summary.bytes_uncompressed)
    );
    println!(
        "  on disk        : {}",
        human_bytes(summary.bytes_compressed)
    );
    println!(
        "  approx tokens  : {}",
        count(summary.words.saturating_mul(4) / 3)
    );
    if !summary.rejects.is_empty() {
        println!("Rejections");
        let mut rejects = summary.rejects;
        rejects.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (reason, value) in rejects {
            println!("  {reason:<24} {value}");
        }
    }
    Ok(())
}

fn run_verify(args: VerifyArgs) -> Result<()> {
    let report = verify_dataset(&args.output_dir, &args.manifest).with_context(|| {
        format!(
            "cannot verify dataset at {} (manifest {})",
            args.output_dir.display(),
            args.manifest
        )
    })?;
    println!("Shards checked : {}", report.shards);
    println!("Documents      : {}", count(report.documents));
    println!("Words          : {}", count(report.words));
    println!("Repeated lines : {}", count(report.duplicate_lines));
    println!("Duplicate blocks: {}", count(report.duplicate_blocks));
    if report.is_ok() {
        println!("Result         : OK (all SHA-256 hashes and records are valid)");
        return Ok(());
    }
    for failure in &report.failures {
        eprintln!("FAIL {failure}");
    }
    bail!(
        "verification failed with {} problem(s)",
        report.failures.len()
    )
}

fn run_export_txt(args: ExportTxtArgs) -> Result<()> {
    let output = args
        .output
        .unwrap_or_else(|| args.dataset.join("corpus.txt"));
    let options = ExportOptions {
        separator: args.separator,
        include_relevant_title: !args.no_title,
        max_chunk_words: args.max_chunk_words,
        min_chunk_words: args.min_chunk_words,
        deduplicate: !args.no_dedupe,
        dedup_min_sentence_words: args.dedup_min_sentence_words,
        dedup_index_capacity: args.dedup_index_capacity,
    };
    let report = export_txt(&args.dataset, &args.manifest, &output, &options)
        .with_context(|| format!("cannot export {} as plain text", args.dataset.display()))?;
    println!("Plain-text corpus      : {}", output.display());
    println!("  shards read          : {}", report.shards);
    println!("  documents            : {}", count(report.documents));
    println!("  words                : {}", count(report.words));
    println!("  size                 : {}", human_bytes(report.bytes));
    println!(
        "  duplicates removed   : {}",
        count(report.duplicate_units_removed)
    );
    println!(
        "  titles kept / dropped: {} / {}",
        count(report.titles_included),
        count(report.titles_dropped)
    );
    println!("  separator            : {}", options.separator);
    Ok(())
}

fn read_seeds(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read seeds file {}", path.display()))?;
    let seeds: Vec<String> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    if seeds.is_empty() {
        bail!("seeds file {} contains no URLs", path.display());
    }
    Ok(seeds)
}

fn init_logging(config: &Config) {
    let filter = tracing_subscriber::EnvFilter::try_new(&config.general.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if config.general.json_logs {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_target(true)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }
}

fn print_summary(kind: &str, snapshot: &StatsSnapshot, manifest_path: &Path) {
    println!("{kind} finished in {:.1}s", snapshot.elapsed_secs);
    println!("  fetched          : {}", count(snapshot.fetched));
    println!("  fetch errors     : {}", count(snapshot.fetch_errors));
    println!("  robots blocked   : {}", count(snapshot.robots_blocked));
    println!("  extracted        : {}", count(snapshot.extracted));
    println!(
        "  documents        : {} in {} shard(s)",
        count(snapshot.accepted_docs),
        count(snapshot.shards)
    );
    println!("  words            : {}", count(snapshot.accepted_words));
    println!("  records          : {}", count(snapshot.records));
    println!("  duplicates cut   : {}", count(snapshot.dedup_removed));
    println!(
        "  text size        : {} ({} on disk)",
        human_bytes(snapshot.accepted_bytes),
        human_bytes(snapshot.compressed_bytes)
    );
    println!("  manifest         : {}", manifest_path.display());
    let rejects: Vec<(String, u64)> = snapshot
        .rejects
        .iter()
        .filter(|(_, value)| *value > 0)
        .cloned()
        .collect();
    if !rejects.is_empty() {
        let total: u64 = rejects.iter().map(|(_, value)| value).sum();
        println!("  rejected         : {}", count(total));
        for (reason, value) in rejects {
            println!("    {reason:<24} {value}");
        }
    }
}

fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}
