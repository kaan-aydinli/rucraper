//! TOML configuration model with defaults and strict validation.
//!
//! `serde(default)` keeps the example file short, `deny_unknown_fields` turns a
//! typo in a key into a hard error instead of a silently ignored setting, and
//! [`Config::validate`] rejects values that would produce a useless (or unsafe)
//! crawl before a single byte hits the network.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Result, ScraperError};

/// Root configuration object, mirroring the section layout of `config.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Global paths and logging behaviour.
    pub general: GeneralConfig,
    /// URL frontier, politeness and crawl scope.
    pub frontier: FrontierConfig,
    /// HTTP client behaviour, size limits and retries.
    pub fetch: FetchConfig,
    /// HTML pruning, density scoring and Markdown rendering.
    pub extract: ExtractConfig,
    /// Text quality, repetition, language and PII settings.
    pub filter: FilterConfig,
    /// Exact and near-duplicate detection settings.
    pub dedup: DedupConfig,
    /// Shard output format.
    pub output: OutputConfig,
    /// Concurrency and backpressure tuning.
    pub pipeline: PipelineConfig,
}

/// Global paths, logging and telemetry toggles.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GeneralConfig {
    /// Directory that receives `shard_XXXXX.jsonl.zst` and `manifest.json`.
    pub output_dir: PathBuf,
    /// Directory of the embedded LSM database holding frontier/dedup state.
    pub db_path: PathBuf,
    /// `tracing` filter directive (e.g. `info`, `pretrain_scraper=debug`).
    pub log_level: String,
    /// Emit structured JSON logs instead of the human readable format.
    pub json_logs: bool,
    /// Draw the live `indicatif` dashboard on stderr.
    pub progress: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("dataset"),
            db_path: PathBuf::from("crawl_db"),
            log_level: "info".to_string(),
            json_logs: false,
            progress: true,
        }
    }
}

/// Frontier scope, politeness and queue limits.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FrontierConfig {
    /// If non-empty, only these registrable domains are crawled.
    pub allowed_domains: Vec<String>,
    /// Text file with one site per line (`https://example.com/` or a bare
    /// domain, `#` comments). When set, its registrable domains are added to
    /// `allowed_domains` and its URLs become the default seed list.
    pub allowed_domains_file: PathBuf,
    /// Never crawled, even when linked from an allowed page.
    pub blocked_domains: Vec<String>,
    /// Maximum link depth counted from the seeds (`0` = seeds only).
    pub max_depth: u32,
    /// Minimum delay between two requests to the same domain.
    pub min_delay_ms: u64,
    /// Maximum concurrent requests per domain.
    pub max_concurrency_per_domain: usize,
    /// Ignore `robots.txt` completely (only for corpora you own).
    pub ignore_robots: bool,
    /// Honour `Crawl-delay` from `robots.txt` when it exceeds `min_delay_ms`.
    pub respect_crawl_delay: bool,
    /// Read sitemaps advertised in `robots.txt` before crawling. Disable it for
    /// large site lists: discovering every sitemap delays the first fetch a lot,
    /// while depth-based link discovery already covers the same sites.
    pub discover_sitemaps: bool,
    /// Lifetime of a cached `robots.txt` document.
    pub robots_ttl_secs: u64,
    /// How many pending URLs are kept in the database before backpressure kicks in.
    pub queue_capacity: usize,
    /// Hard cap on pages fetched per run (`0` = unlimited).
    pub max_pages: u64,
    /// Priority assigned to seeds (higher value = popped first).
    pub seed_priority: u8,
    /// Seeds read from `allowed_domains_file` (filled by [`Config::load`]).
    #[serde(skip)]
    pub site_seeds: Vec<String>,
}

impl Default for FrontierConfig {
    fn default() -> Self {
        Self {
            allowed_domains: Vec::new(),
            allowed_domains_file: PathBuf::new(),
            blocked_domains: Vec::new(),
            max_depth: 3,
            min_delay_ms: 1_000,
            max_concurrency_per_domain: 2,
            ignore_robots: false,
            respect_crawl_delay: true,
            discover_sitemaps: true,
            robots_ttl_secs: 86_400,
            queue_capacity: 100_000,
            max_pages: 0,
            seed_priority: 200,
            site_seeds: Vec::new(),
        }
    }
}

/// HTTP client behaviour.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FetchConfig {
    /// Transparent, realistic User-Agent; also matched against `robots.txt`.
    pub user_agent: String,
    /// `Accept-Language` header value sent with every request.
    pub accept_language: String,
    /// Per-request timeout.
    pub request_timeout_secs: u64,
    /// Hard cap for a single response body (streaming, never exceeded).
    pub max_response_bytes: u64,
    /// Additional attempts after the first failure for retryable errors.
    pub max_retries: u32,
    /// Base delay for exponential backoff.
    pub retry_base_delay_ms: u64,
    /// Redirect hop limit.
    pub max_redirects: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            user_agent:
                "PretrainScraperBot/0.1 (+https://example.org/bot; research corpus builder)"
                    .to_string(),
            accept_language: "en-US,en;q=0.9,tr;q=0.8".to_string(),
            request_timeout_secs: 30,
            max_response_bytes: 5 * 1024 * 1024,
            max_retries: 3,
            retry_base_delay_ms: 500,
            max_redirects: 10,
        }
    }
}

/// Extraction tuning.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractConfig {
    /// A candidate block with less text than this is not considered an article body.
    pub min_block_chars: usize,
    /// Blocks whose link-text ratio exceeds this are treated as link farms.
    pub max_link_ratio: f64,
    /// Render `<table>` elements as pipe separated text.
    pub keep_tables: bool,
    /// Render `<pre>`/`<code>` as fenced code blocks.
    pub keep_code_blocks: bool,
    /// Upper bound of links discovered per page.
    pub max_links_per_page: usize,
    /// Maximum length of the extracted title in characters.
    pub max_title_chars: usize,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            min_block_chars: 80,
            max_link_ratio: 0.45,
            keep_tables: true,
            keep_code_blocks: true,
            max_links_per_page: 2_000,
            max_title_chars: 300,
        }
    }
}

/// Quality filter thresholds (FineWeb / RefinedWeb / Gopher derived).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FilterConfig {
    /// Documents with fewer words are rejected (short news briefs are kept).
    pub min_words: usize,
    /// Documents with more words are rejected (only pathological dumps).
    pub max_words: usize,
    /// Lower bound for the average word length in characters.
    pub min_avg_word_len: f64,
    /// Upper bound for the average word length in characters.
    pub max_avg_word_len: f64,
    /// Fraction of non-empty lines that must end in sentence punctuation.
    pub min_punctuation_line_ratio: f64,
    /// Fraction of words that must contain at least one alphabetic character.
    pub min_alpha_word_ratio: f64,
    /// Maximum share of symbol characters relative to word count.
    pub max_symbol_word_ratio: f64,
    /// Maximum fraction of duplicated non-empty lines.
    pub max_duplicate_line_ratio: f64,
    /// Maximum share of the text taken by the single most frequent n-gram.
    pub max_ngram_repetition_ratio: f64,
    /// n-gram width used by the repetition filter.
    pub ngram_size: usize,
    /// Accepted ISO-639-1 (or ISO-639-3) language codes.
    pub target_languages: Vec<String>,
    /// Minimum `whatlang` confidence for a document to be accepted.
    pub min_lang_confidence: f64,
    /// Enable / disable the language filter entirely.
    pub enable_language_filter: bool,
    /// Mask e-mails, phones, IPs and credit card numbers.
    pub enable_pii_masking: bool,
    /// Reject pages that contain any of these phrases (case-insensitive).
    pub blocked_phrases: Vec<String>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            min_words: 50,
            max_words: 1_000_000,
            min_avg_word_len: 3.0,
            max_avg_word_len: 12.0,
            min_punctuation_line_ratio: 0.6,
            min_alpha_word_ratio: 0.8,
            max_symbol_word_ratio: 0.1,
            max_duplicate_line_ratio: 0.25,
            max_ngram_repetition_ratio: 0.3,
            ngram_size: 5,
            target_languages: vec!["en".into(), "tr".into()],
            min_lang_confidence: 0.85,
            enable_language_filter: true,
            enable_pii_masking: true,
            blocked_phrases: vec![
                "lorem ipsum".into(),
                "javascript is required".into(),
                "please enable cookies".into(),
                "enable javascript".into(),
                "404 not found".into(),
                "access denied".into(),
                "just a moment".into(),
                "cloudflare".into(),
                "subscribe to continue".into(),
                "sign in to continue".into(),
            ],
        }
    }
}

/// Deduplication tuning.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DedupConfig {
    /// Enable the persistent XXH3-128 exact duplicate index.
    pub enable_exact: bool,
    /// Enable the persistent SimHash + LSH near duplicate index.
    pub enable_near_dup: bool,
    /// Maximum Hamming distance still counted as a near duplicate.
    pub max_hamming_distance: u32,
    /// Words per shingle used to build the SimHash fingerprint.
    pub shingle_size: usize,
    /// A paragraph repeated more often than this inside one run is stripped.
    pub paragraph_dedup_threshold: u32,
    /// Size bound of the in-memory paragraph counter (keeps memory flat).
    pub paragraph_dedup_max_entries: usize,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            enable_exact: true,
            enable_near_dup: true,
            max_hamming_distance: 3,
            shingle_size: 5,
            paragraph_dedup_threshold: 3,
            paragraph_dedup_max_entries: 65_536,
        }
    }
}

/// On-disk shard format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Plain-text pretraining corpus: one document after another, separated by
    /// the configured marker. This is the default.
    #[default]
    Txt,
    /// Compressed JSONL records with the full metadata schema (audit format).
    Jsonl,
}

impl OutputFormat {
    /// Stable identifier used in the manifest.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Txt => "txt",
            OutputFormat::Jsonl => "jsonl",
        }
    }
}

/// Shard writer settings.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Shard format: `txt` (plain corpus) or `jsonl` (records).
    pub format: OutputFormat,
    /// Marker written on its own line after every document in `txt` mode.
    pub separator: String,
    /// Write the title in `txt` mode only when it matches the body text.
    pub include_title: bool,
    /// Largest topic block in `txt` mode, in words.
    pub max_chunk_words: u64,
    /// Blocks shorter than this are merged with the following section.
    /// Kept low so short but complete sections survive as their own record.
    pub min_chunk_words: u64,
    /// Remove paragraphs, sentences and blocks already written earlier.
    pub deduplicate: bool,
    /// Shortest sentence/paragraph checked for duplication.
    pub dedup_min_sentence_words: usize,
    /// Size bound of the corpus-wide duplicate index (constant memory).
    pub dedup_index_capacity: usize,
    /// Uncompressed size at which a shard is finished and rotated.
    pub shard_size_bytes: u64,
    /// Zstandard compression level (1-22), used by the `jsonl` format.
    pub zstd_level: i32,
    /// How often pending state is flushed to disk (seconds).
    pub flush_interval_secs: u64,
    /// Manifest file name inside `output_dir`.
    pub manifest_name: String,
    /// File inside `output_dir` receiving the full URL of every saved content
    /// page. Later crawls skip these pages before fetching them. Empty = off.
    pub saved_urls_file: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: OutputFormat::Txt,
            separator: "<veribitisi>".to_string(),
            include_title: true,
            max_chunk_words: 3_000,
            min_chunk_words: 25,
            deduplicate: true,
            dedup_min_sentence_words: 6,
            dedup_index_capacity: 500_000,
            shard_size_bytes: 256 * 1024 * 1024,
            zstd_level: 5,
            flush_interval_secs: 15,
            manifest_name: "manifest.json".to_string(),
            saved_urls_file: "crawled_urls.txt".to_string(),
        }
    }
}

/// Concurrency and channel sizing (all channels are bounded).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    /// Capacity of every inter-stage `mpsc` channel.
    pub channel_capacity: usize,
    /// Number of concurrent HTTP fetch tasks.
    pub fetch_workers: usize,
    /// Parallel extraction tasks (`0` = number of CPUs).
    pub extract_workers: usize,
    /// Parallel filter tasks (`0` = number of CPUs).
    pub filter_workers: usize,
    /// Parallel dedup tasks (`0` = half the CPUs, min 1).
    pub dedup_workers: usize,
    /// Dashboard refresh interval.
    pub dashboard_interval_ms: u64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 256,
            fetch_workers: 32,
            extract_workers: 0,
            filter_workers: 0,
            dedup_workers: 0,
            dashboard_interval_ms: 500,
        }
    }
}

impl PipelineConfig {
    /// Resolved extraction worker count.
    pub fn resolved_extract_workers(&self) -> usize {
        resolve_workers(self.extract_workers, 1)
    }

    /// Resolved filter worker count.
    pub fn resolved_filter_workers(&self) -> usize {
        resolve_workers(self.filter_workers, 1)
    }

    /// Resolved dedup worker count (defaults to half the machine, at least 1).
    pub fn resolved_dedup_workers(&self) -> usize {
        if self.dedup_workers > 0 {
            self.dedup_workers
        } else {
            (available_parallelism() / 2).max(1)
        }
    }

    /// Resolved fetch worker count.
    pub fn resolved_fetch_workers(&self) -> usize {
        resolve_workers(self.fetch_workers, 1)
    }
}

fn resolve_workers(configured: usize, minimum: usize) -> usize {
    if configured > 0 {
        configured
    } else {
        available_parallelism().max(minimum)
    }
}

/// Number of usable CPUs, clamped to a sane range.
pub fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 256)
}

impl Config {
    /// Load and validate a TOML configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| ScraperError::io_at(path, e))?;
        let mut config: Config = toml::from_str(&raw)?;
        config.load_site_list(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Read `frontier.allowed_domains_file` (relative to the config file) into
    /// the crawl scope and the default seed list.
    fn load_site_list(&mut self, config_path: &Path) -> Result<()> {
        if self.frontier.allowed_domains_file.as_os_str().is_empty() {
            return Ok(());
        }
        let base = config_path.parent().unwrap_or_else(|| Path::new("."));
        let list_path = base.join(&self.frontier.allowed_domains_file);
        let raw =
            std::fs::read_to_string(&list_path).map_err(|e| ScraperError::io_at(&list_path, e))?;
        let mut domains = 0usize;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parsed = match Url::parse(line) {
                Ok(url) => url,
                Err(_) => match Url::parse(&format!("https://{line}")) {
                    Ok(url) => url,
                    Err(_) => {
                        return Err(ScraperError::config(format!(
                            "invalid site entry {line:?} in {}",
                            list_path.display()
                        )));
                    }
                },
            };
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ScraperError::config(format!(
                    "site entry {line:?} in {} is not an http(s) URL",
                    list_path.display()
                )));
            }
            let Some(host) = parsed.host_str().map(|host| host.to_ascii_lowercase()) else {
                return Err(ScraperError::config(format!(
                    "site entry {line:?} in {} has no host",
                    list_path.display()
                )));
            };
            // Keep a listed subdomain scoped to itself (`tr.wikipedia.org`),
            // but fold `www.` so links to the bare domain stay in scope.
            let domain = host.strip_prefix("www.").unwrap_or(&host).to_string();
            if !self.frontier.allowed_domains.contains(&domain) {
                self.frontier.allowed_domains.push(domain);
                domains += 1;
            }
            let seed = if parsed.path() == "/" || parsed.path().is_empty() {
                format!("{}://{}/", parsed.scheme(), parsed.host_str().unwrap_or(""))
            } else {
                parsed.to_string()
            };
            if !self.frontier.site_seeds.contains(&seed) {
                self.frontier.site_seeds.push(seed);
            }
        }
        if domains == 0 && self.frontier.site_seeds.is_empty() {
            return Err(ScraperError::config(format!(
                "site list {} is empty",
                list_path.display()
            )));
        }
        Ok(())
    }

    /// Parse a configuration from a TOML string (used by tests).
    pub fn from_toml(raw: &str) -> Result<Self> {
        let config: Config = toml::from_str(raw)?;
        config.validate()?;
        Ok(config)
    }

    /// Semantic validation; returns [`ScraperError::Config`] on the first problem.
    pub fn validate(&self) -> Result<()> {
        if self.general.log_level.trim().is_empty() {
            return Err(ScraperError::config("general.log_level must not be empty"));
        }
        if self.general.db_path.as_os_str().is_empty() {
            return Err(ScraperError::config("general.db_path must not be empty"));
        }
        if self.general.output_dir.as_os_str().is_empty() {
            return Err(ScraperError::config("general.output_dir must not be empty"));
        }
        if self.frontier.max_concurrency_per_domain == 0 {
            return Err(ScraperError::config(
                "frontier.max_concurrency_per_domain must be >= 1",
            ));
        }
        if self.frontier.queue_capacity == 0 {
            return Err(ScraperError::config("frontier.queue_capacity must be >= 1"));
        }
        if self.frontier.max_depth > 64 {
            return Err(ScraperError::config(
                "frontier.max_depth above 64 is not supported",
            ));
        }
        for domain in self
            .frontier
            .allowed_domains
            .iter()
            .chain(self.frontier.blocked_domains.iter())
        {
            if domain.trim().is_empty() || domain.contains('/') || domain.contains(' ') {
                return Err(ScraperError::config(format!(
                    "invalid domain entry in frontier filters: {domain:?}"
                )));
            }
        }
        if self.fetch.user_agent.trim().is_empty() {
            return Err(ScraperError::config("fetch.user_agent must not be empty"));
        }
        if self.fetch.max_response_bytes < 1024 {
            return Err(ScraperError::config(
                "fetch.max_response_bytes must be at least 1024",
            ));
        }
        if self.fetch.request_timeout_secs == 0 {
            return Err(ScraperError::config(
                "fetch.request_timeout_secs must be >= 1",
            ));
        }
        if !(0.0..=1.0).contains(&self.extract.max_link_ratio) {
            return Err(ScraperError::config(
                "extract.max_link_ratio must be within [0.0, 1.0]",
            ));
        }
        if self.filter.min_words < 5 {
            return Err(ScraperError::config("filter.min_words must be >= 5"));
        }
        if self.filter.min_words > self.filter.max_words {
            return Err(ScraperError::config(
                "filter.min_words must not exceed filter.max_words",
            ));
        }
        if !(1.0..=64.0).contains(&self.filter.min_avg_word_len) {
            return Err(ScraperError::config(
                "filter.min_avg_word_len must be within [1.0, 64.0]",
            ));
        }
        if self.filter.max_avg_word_len < self.filter.min_avg_word_len {
            return Err(ScraperError::config(
                "filter.max_avg_word_len must be >= filter.min_avg_word_len",
            ));
        }
        for (name, value) in [
            (
                "filter.min_punctuation_line_ratio",
                self.filter.min_punctuation_line_ratio,
            ),
            (
                "filter.min_alpha_word_ratio",
                self.filter.min_alpha_word_ratio,
            ),
            (
                "filter.max_symbol_word_ratio",
                self.filter.max_symbol_word_ratio,
            ),
            (
                "filter.max_duplicate_line_ratio",
                self.filter.max_duplicate_line_ratio,
            ),
            (
                "filter.max_ngram_repetition_ratio",
                self.filter.max_ngram_repetition_ratio,
            ),
            (
                "filter.min_lang_confidence",
                self.filter.min_lang_confidence,
            ),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ScraperError::config(format!(
                    "{name} must be within [0.0, 1.0]"
                )));
            }
        }
        if self.filter.ngram_size < 2 {
            return Err(ScraperError::config("filter.ngram_size must be >= 2"));
        }
        if self.filter.enable_language_filter {
            if self.filter.target_languages.is_empty() {
                return Err(ScraperError::config(
                    "filter.target_languages must not be empty when the language filter is enabled",
                ));
            }
            for lang in &self.filter.target_languages {
                if !(2..=3).contains(&lang.len()) || !lang.chars().all(|c| c.is_ascii_alphabetic())
                {
                    return Err(ScraperError::config(format!(
                        "invalid ISO language code {lang:?} in filter.target_languages"
                    )));
                }
            }
        }
        if self.dedup.max_hamming_distance > 64 {
            return Err(ScraperError::config(
                "dedup.max_hamming_distance must be <= 64",
            ));
        }
        if self.dedup.shingle_size == 0 {
            return Err(ScraperError::config("dedup.shingle_size must be >= 1"));
        }
        if self.dedup.paragraph_dedup_threshold == 0 {
            return Err(ScraperError::config(
                "dedup.paragraph_dedup_threshold must be >= 1",
            ));
        }
        if self.dedup.paragraph_dedup_max_entries < 128 {
            return Err(ScraperError::config(
                "dedup.paragraph_dedup_max_entries must be >= 128",
            ));
        }
        if self.output.shard_size_bytes < 1024 {
            return Err(ScraperError::config(
                "output.shard_size_bytes must be at least 1024",
            ));
        }
        if matches!(self.output.format, OutputFormat::Jsonl)
            && !(1..=22).contains(&self.output.zstd_level)
        {
            return Err(ScraperError::config(
                "output.zstd_level must be within [1, 22]",
            ));
        }
        if self.output.separator.trim().is_empty() {
            return Err(ScraperError::config("output.separator must not be empty"));
        }
        if self.output.separator.contains(['\n', '\r']) {
            return Err(ScraperError::config(
                "output.separator must be a single line",
            ));
        }
        if self.output.min_chunk_words == 0 {
            return Err(ScraperError::config(
                "output.min_chunk_words must be positive",
            ));
        }
        if self.output.max_chunk_words < self.output.min_chunk_words {
            return Err(ScraperError::config(
                "output.max_chunk_words must be >= output.min_chunk_words",
            ));
        }
        if self.output.max_chunk_words > 100_000 {
            return Err(ScraperError::config(
                "output.max_chunk_words must be <= 100000",
            ));
        }
        if self.output.dedup_min_sentence_words < 3 {
            return Err(ScraperError::config(
                "output.dedup_min_sentence_words must be >= 3",
            ));
        }
        if self.output.dedup_index_capacity < 1_024 {
            return Err(ScraperError::config(
                "output.dedup_index_capacity must be >= 1024",
            ));
        }
        if self.output.manifest_name.trim().is_empty()
            || self.output.manifest_name.contains('/')
            || self.output.manifest_name.contains("..")
        {
            return Err(ScraperError::config(
                "output.manifest_name must be a plain file name",
            ));
        }
        if !self.output.saved_urls_file.trim().is_empty()
            && (self.output.saved_urls_file.contains('/')
                || self.output.saved_urls_file.contains(".."))
        {
            return Err(ScraperError::config(
                "output.saved_urls_file must be a plain file name",
            ));
        }
        if self.pipeline.channel_capacity == 0 {
            return Err(ScraperError::config(
                "pipeline.channel_capacity must be >= 1",
            ));
        }
        if self.pipeline.dashboard_interval_ms < 50 {
            return Err(ScraperError::config(
                "pipeline.dashboard_interval_ms must be >= 50",
            ));
        }
        Ok(())
    }

    /// Ensure `output_dir` and `db_path` exist.
    pub fn prepare_directories(&self) -> Result<()> {
        std::fs::create_dir_all(&self.general.output_dir)
            .map_err(|e| ScraperError::io_at(&self.general.output_dir, e))?;
        std::fs::create_dir_all(&self.general.db_path)
            .map_err(|e| ScraperError::io_at(&self.general.db_path, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().expect("defaults are valid");
    }

    #[test]
    fn example_file_matches_schema() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let config = Config::load(&path).expect("example config parses");
        assert_eq!(config.filter.min_words, 50);
        assert_eq!(config.output.shard_size_bytes, 268_435_456);
        assert!(config.general.output_dir.ends_with("dataset"));
    }

    #[test]
    fn site_list_file_supplies_scope_and_seeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let list = dir.path().join("sites.txt");
        std::fs::write(
            &list,
            "# Turkish sites\nhttps://example.org/\nwww.example.com\nexample.net/dergi\n",
        )
        .expect("write list");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[frontier]\nallowed_domains_file = \"sites.txt\"\n",
        )
        .expect("write config");
        let config = Config::load(&config_path).expect("config loads");
        assert!(config
            .frontier
            .allowed_domains
            .contains(&"example.org".to_string()));
        assert!(config
            .frontier
            .allowed_domains
            .contains(&"example.com".to_string()));
        assert!(config
            .frontier
            .allowed_domains
            .contains(&"example.net".to_string()));
        assert_eq!(config.frontier.site_seeds.len(), 3);
        assert_eq!(config.frontier.site_seeds[0], "https://example.org/");
        assert_eq!(config.frontier.site_seeds[1], "https://www.example.com/");
        assert!(config.frontier.site_seeds[2].ends_with("/dergi"));
    }

    #[test]
    fn defaults_accept_short_and_long_documents() {
        let config = Config::default();
        assert_eq!(config.filter.min_words, 50);
        assert!(config.filter.max_words >= 1_000_000);
        assert_eq!(config.output.min_chunk_words, 25);
        assert_eq!(config.output.max_chunk_words, 3_000);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::from_toml("[filter]\nmin_wrods = 10\n").expect_err("typo rejected");
        assert!(err.to_string().contains("unknown field"), "got: {err}");
    }

    #[test]
    fn semantic_validation_catches_bad_ranges() {
        let config = Config {
            filter: FilterConfig {
                min_words: 500,
                max_words: 100,
                ..FilterConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());

        let config = Config {
            output: OutputConfig {
                format: OutputFormat::Jsonl,
                zstd_level: 99,
                ..OutputConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());

        let config = Config {
            output: OutputConfig {
                separator: String::new(),
                ..OutputConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());

        let config = Config {
            filter: FilterConfig {
                target_languages: vec!["english".into()],
                ..FilterConfig::default()
            },
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn output_format_defaults_to_txt_and_parses_jsonl() {
        assert_eq!(Config::default().output.format, OutputFormat::Txt);
        let config = Config::from_toml("[output]\nformat = \"jsonl\"\n").expect("jsonl parses");
        assert_eq!(config.output.format, OutputFormat::Jsonl);
        assert!(Config::from_toml("[output]\nformat = \"xml\"\n").is_err());
        assert_eq!(OutputFormat::Txt.as_str(), "txt");
    }

    #[test]
    fn worker_counts_are_resolved() {
        let pipeline = PipelineConfig {
            extract_workers: 0,
            ..PipelineConfig::default()
        };
        assert!(pipeline.resolved_extract_workers() >= 1);
        let pipeline = PipelineConfig {
            extract_workers: 7,
            ..PipelineConfig::default()
        };
        assert_eq!(pipeline.resolved_extract_workers(), 7);
        assert!(pipeline.resolved_dedup_workers() >= 1);
    }
}
