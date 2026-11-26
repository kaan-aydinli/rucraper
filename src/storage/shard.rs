//! Rotating shard writer with an atomically replaced manifest.
//!
//! Two on-disk layouts are supported:
//!
//! * `txt` (default) — plain-text pretraining shards `corpus_XXXXX.txt`. Every
//!   document is written as `[title\n\n]body\n<separator>\n\n`, so a shard is a
//!   ready-to-train corpus slice;
//! * `jsonl` — Zstandard-compressed records `shard_XXXXX.jsonl.zst` with the
//!   full metadata schema.
//!
//! A shard is finished, fsynced, hashed with SHA-256 and then recorded. The
//! manifest is always written to `<name>.tmp` and renamed over the real file, so
//! a crash can never leave a half-written manifest: readers either see the
//! previous version or the complete new one.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{OutputConfig, OutputFormat};
use crate::error::{Result, ScraperError};
use crate::export::{format_markdown_blocks, ChunkOptions, CorpusDeduper, DEFAULT_SEPARATOR};
use crate::storage::{sha256_file, Document};
use crate::telemetry::Stats;

/// Current manifest schema version.
pub const MANIFEST_VERSION: u32 = 2;

/// One finished shard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShardMeta {
    /// File name inside the output directory.
    pub name: String,
    /// Size in bytes on disk.
    pub size_bytes: u64,
    /// Uncompressed payload size in bytes (equal to `size_bytes` for `txt`).
    pub uncompressed_bytes: u64,
    /// Number of documents.
    pub documents: u64,
    /// Total words across those documents.
    pub words: u64,
    /// SHA-256 of the file on disk.
    pub sha256: String,
}

/// `manifest.json` content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// Schema version.
    pub version: u32,
    /// Shard layout: `txt` or `jsonl`.
    #[serde(default)]
    pub format: String,
    /// Document separator used by `txt` shards.
    #[serde(default = "default_separator")]
    pub separator: String,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
    /// Configured shard rotation size.
    pub shard_size_bytes: u64,
    /// Total documents across shards.
    pub documents: u64,
    /// Total words across shards.
    pub words: u64,
    /// Total uncompressed bytes across shards.
    pub bytes_uncompressed: u64,
    /// Finished shards, in creation order.
    pub shards: Vec<ShardMeta>,
    /// Rejections by reason code.
    pub rejects: BTreeMap<String, u64>,
}

fn default_separator() -> String {
    DEFAULT_SEPARATOR.to_string()
}

impl Manifest {
    /// Fresh manifest for a new dataset.
    pub fn fresh(shard_size_bytes: u64, format: OutputFormat, separator: &str) -> Self {
        let now = now_rfc3339();
        Self {
            version: MANIFEST_VERSION,
            format: format.as_str().to_string(),
            separator: separator.to_string(),
            created_at: now.clone(),
            updated_at: now,
            shard_size_bytes,
            documents: 0,
            words: 0,
            bytes_uncompressed: 0,
            shards: Vec::new(),
            rejects: BTreeMap::new(),
        }
    }
}

enum ActiveWriter {
    Txt(BufWriter<File>),
    Jsonl(zstd::stream::write::Encoder<'static, File>),
}

impl ActiveWriter {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            ActiveWriter::Txt(writer) => writer.write_all(bytes),
            ActiveWriter::Jsonl(encoder) => encoder.write_all(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ActiveWriter::Txt(writer) => writer.flush(),
            ActiveWriter::Jsonl(encoder) => encoder.flush(),
        }
    }

    fn finish(self) -> std::io::Result<File> {
        match self {
            ActiveWriter::Txt(mut writer) => {
                writer.flush()?;
                writer.into_inner().map_err(|error| error.into_error())
            }
            ActiveWriter::Jsonl(encoder) => encoder.finish(),
        }
    }
}

struct ActiveShard {
    name: String,
    path: PathBuf,
    writer: Option<ActiveWriter>,
    uncompressed: u64,
    documents: u64,
    words: u64,
    since_flush: u64,
}

/// Writes documents into rotating shards and maintains the manifest.
pub struct ShardWriter {
    output_dir: PathBuf,
    manifest_path: PathBuf,
    shard_size_bytes: u64,
    zstd_level: i32,
    format: OutputFormat,
    separator: String,
    chunk_options: ChunkOptions,
    deduper: Option<CorpusDeduper>,
    stats: Arc<Stats>,
    manifest: Manifest,
    base_rejects: BTreeMap<String, u64>,
    active: Option<ActiveShard>,
    next_index: u32,
}

impl ShardWriter {
    /// Open (or resume) a dataset directory.
    pub fn new(output_dir: &Path, config: &OutputConfig, stats: Arc<Stats>) -> Result<Self> {
        std::fs::create_dir_all(output_dir).map_err(|e| ScraperError::io_at(output_dir, e))?;
        let manifest_path = output_dir.join(&config.manifest_name);
        let (manifest, base_rejects) =
            match crate::storage::load_manifest(output_dir, &config.manifest_name)? {
                Some(manifest) => {
                    let base = manifest.rejects.clone();
                    (manifest, base)
                }
                None => (
                    Manifest::fresh(config.shard_size_bytes, config.format, &config.separator),
                    BTreeMap::new(),
                ),
            };
        if !manifest.format.is_empty() && manifest.format != config.format.as_str() {
            tracing::warn!(
                existing = %manifest.format,
                configured = %config.format.as_str(),
                "dataset directory already holds another shard format; new shards use the configured one"
            );
        }
        let next_index = next_shard_index(output_dir);
        Ok(Self {
            output_dir: output_dir.to_path_buf(),
            manifest_path,
            shard_size_bytes: config.shard_size_bytes.max(1024),
            zstd_level: config.zstd_level.clamp(1, 22),
            format: config.format,
            separator: config.separator.clone(),
            chunk_options: ChunkOptions {
                include_relevant_title: config.include_title,
                separator: config.separator.clone(),
                max_words: config.max_chunk_words.max(1),
                min_words: config.min_chunk_words.min(config.max_chunk_words.max(1)),
            },
            deduper: config.deduplicate.then(|| {
                CorpusDeduper::new(config.dedup_index_capacity, config.dedup_min_sentence_words)
            }),
            stats,
            manifest,
            base_rejects,
            active: None,
            next_index,
        })
    }

    /// Path of the manifest file.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Append one document as one or more topic-separated records.
    /// Write one document; `Ok(false)` means nothing reached the shard (every
    /// block/record was a duplicate), so the page URL must not be marked saved.
    pub fn write(&mut self, document: &Document) -> Result<bool> {
        match self.format {
            OutputFormat::Txt => {
                // Duplicate paragraphs, sentences and blocks are removed across
                // the whole run, not only inside one page, so republished text
                // and repeated lead paragraphs never reach the shard twice.
                let removed_before = self
                    .deduper
                    .as_ref()
                    .map(CorpusDeduper::removed)
                    .unwrap_or(0);
                let cleaned = self
                    .deduper
                    .as_mut()
                    .map(|deduper| deduper.clean_markdown(&document.text));
                let markdown = cleaned.as_deref().unwrap_or(&document.text);
                let blocks = format_markdown_blocks(&document.title, markdown, &self.chunk_options);
                let removed = self
                    .deduper
                    .as_ref()
                    .map(CorpusDeduper::removed)
                    .unwrap_or(0);
                if removed > removed_before {
                    self.stats.record_dedup(removed - removed_before);
                }
                if blocks.is_empty() {
                    return Ok(false);
                }
                let mut words = 0u64;
                let mut chars = 0u64;
                let mut bytes = 0u64;
                for block in &blocks {
                    self.append(block.text.as_bytes(), block.words)?;
                    words += block.words;
                    chars += block.chars;
                    bytes += block.text.len() as u64;
                }
                self.stats.record_accepted(words, chars, bytes);
                self.stats.record_records(blocks.len() as u64);
            }
            OutputFormat::Jsonl => {
                let mut line = serde_json::to_vec(document)?;
                line.push(b'\n');
                self.append(&line, document.word_count as u64)?;
                self.stats.record_accepted(
                    document.word_count as u64,
                    document.char_count as u64,
                    line.len() as u64,
                );
                self.stats.record_records(1);
            }
        }
        Ok(true)
    }

    /// Append one record to the active shard, rotating when it fills up.
    fn append(&mut self, bytes: &[u8], words: u64) -> Result<()> {
        if self.active.is_none() {
            self.open_next()?;
        }
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| ScraperError::storage("no active shard"))?;
        let writer = active
            .writer
            .as_mut()
            .ok_or_else(|| ScraperError::storage("active shard has no writer"))?;
        writer
            .write_all(bytes)
            .map_err(|e| ScraperError::io_at(&active.path, e))?;
        active.uncompressed += bytes.len() as u64;
        active.documents += 1;
        active.words += words;
        active.since_flush += 1;
        if active.since_flush >= 256 {
            writer
                .flush()
                .map_err(|e| ScraperError::io_at(&active.path, e))?;
            active.since_flush = 0;
        }
        if active.uncompressed >= self.shard_size_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    /// Finish the active shard (if any) and write the manifest.
    pub fn finalize(&mut self) -> Result<()> {
        self.rotate()?;
        self.write_manifest()
    }

    /// Number of documents currently buffered in the open shard.
    pub fn buffered_documents(&self) -> u64 {
        self.active
            .as_ref()
            .map(|active| active.documents)
            .unwrap_or(0)
    }

    fn open_next(&mut self) -> Result<()> {
        let name = match self.format {
            OutputFormat::Txt => format!("corpus_{:05}.txt", self.next_index),
            OutputFormat::Jsonl => format!("shard_{:05}.jsonl.zst", self.next_index),
        };
        self.next_index += 1;
        let path = self.output_dir.join(&name);
        let file = File::create(&path).map_err(|e| ScraperError::io_at(&path, e))?;
        let writer = match self.format {
            OutputFormat::Txt => ActiveWriter::Txt(BufWriter::new(file)),
            OutputFormat::Jsonl => {
                let encoder = zstd::stream::write::Encoder::new(file, self.zstd_level)
                    .map_err(|e| ScraperError::Zstd(format!("{}: {e}", path.display())))?;
                ActiveWriter::Jsonl(encoder)
            }
        };
        self.active = Some(ActiveShard {
            name,
            path,
            writer: Some(writer),
            uncompressed: 0,
            documents: 0,
            words: 0,
            since_flush: 0,
        });
        Ok(())
    }

    fn rotate(&mut self) -> Result<()> {
        let Some(mut active) = self.active.take() else {
            return Ok(());
        };
        let writer = active
            .writer
            .take()
            .ok_or_else(|| ScraperError::storage("active shard has no writer"))?;
        let file = writer
            .finish()
            .map_err(|e| ScraperError::io_at(&active.path, e))?;
        file.sync_all()
            .map_err(|e| ScraperError::io_at(&active.path, e))?;
        drop(file);

        if active.documents == 0 {
            let _ = std::fs::remove_file(&active.path);
            return Ok(());
        }

        let size_bytes = std::fs::metadata(&active.path)
            .map_err(|e| ScraperError::io_at(&active.path, e))?
            .len();
        let sha256 = sha256_file(&active.path)?;
        self.manifest.shards.push(ShardMeta {
            name: active.name,
            size_bytes,
            uncompressed_bytes: active.uncompressed,
            documents: active.documents,
            words: active.words,
            sha256,
        });
        self.stats.record_shard(size_bytes);
        self.write_manifest()
    }

    fn write_manifest(&mut self) -> Result<()> {
        self.manifest.updated_at = now_rfc3339();
        self.manifest.format = self.format.as_str().to_string();
        self.manifest.separator = self.separator.clone();
        self.manifest.documents = self.manifest.shards.iter().map(|s| s.documents).sum();
        self.manifest.words = self.manifest.shards.iter().map(|s| s.words).sum();
        self.manifest.bytes_uncompressed = self
            .manifest
            .shards
            .iter()
            .map(|s| s.uncompressed_bytes)
            .sum();
        let mut rejects = self.base_rejects.clone();
        for (reason, count) in self.stats.reject_breakdown() {
            *rejects.entry(reason).or_insert(0) += count;
        }
        self.manifest.rejects = rejects;
        self.manifest.shard_size_bytes = self.shard_size_bytes;

        let bytes = serde_json::to_vec_pretty(&self.manifest)?;
        let tmp_path = self.manifest_path.with_extension("json.tmp");
        {
            let mut file =
                File::create(&tmp_path).map_err(|e| ScraperError::io_at(&tmp_path, e))?;
            file.write_all(&bytes)
                .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
            file.sync_all()
                .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
        }
        std::fs::rename(&tmp_path, &self.manifest_path)
            .map_err(|e| ScraperError::io_at(&self.manifest_path, e))?;
        Ok(())
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Next free shard index: one past the highest existing shard of either format.
fn next_shard_index(output_dir: &Path) -> u32 {
    let mut highest: Option<u32> = None;
    if let Ok(entries) = std::fs::read_dir(output_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(index) = parse_shard_index(name) {
                highest = Some(highest.map_or(index, |current| current.max(index)));
            }
        }
    }
    highest.map(|index| index + 1).unwrap_or(0)
}

fn parse_shard_index(name: &str) -> Option<u32> {
    if let Some(rest) = name.strip_prefix("shard_") {
        return rest.strip_suffix(".jsonl.zst")?.parse::<u32>().ok();
    }
    let rest = name.strip_prefix("corpus_")?;
    rest.strip_suffix(".txt")?.parse::<u32>().ok()
}

/// List the shard files of a directory, sorted by index.
pub fn list_shards(output_dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(output_dir).map_err(|e| ScraperError::io_at(output_dir, e))?;
    let mut shards: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| ScraperError::io_at(output_dir, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(index) = parse_shard_index(name) {
            shards.push((index, entry.path()));
        }
    }
    shards.sort_by_key(|(index, _)| *index);
    Ok(shards.into_iter().map(|(_, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    use crate::storage::{verify_dataset, Document as StoredDocument};
    use crate::telemetry::Stats;

    fn document(index: usize) -> StoredDocument {
        let text = format!(
            "# Document {index}\n\nThis is body text number {index} written to exercise shard rotation behaviour."
        );
        StoredDocument {
            id: blake3::hash(text.as_bytes()).to_hex().to_string(),
            url: format!("https://example.com/{index}"),
            domain: "example.com".to_string(),
            title: format!("Document {index}"),
            language: "en".to_string(),
            language_score: 0.99,
            word_count: text.split_whitespace().count(),
            char_count: text.chars().count(),
            simhash: format!("0x{:016x}", index as u64 * 7919),
            scraped_at: "2026-11-25T22:30:00Z".to_string(),
            text,
        }
    }

    fn txt_config() -> OutputConfig {
        OutputConfig {
            shard_size_bytes: 1024,
            zstd_level: 3,
            ..OutputConfig::default()
        }
    }

    fn jsonl_config() -> OutputConfig {
        OutputConfig {
            format: OutputFormat::Jsonl,
            shard_size_bytes: 1024,
            zstd_level: 3,
            ..OutputConfig::default()
        }
    }

    #[test]
    fn txt_is_the_default_and_rotation_produces_verifiable_corpora() {
        let dir = tempfile::tempdir().unwrap();
        let output = txt_config();
        assert_eq!(output.format, OutputFormat::Txt);
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &output, stats).unwrap();
        for index in 0..40 {
            writer.write(&document(index)).unwrap();
        }
        writer.finalize().unwrap();

        let shards = list_shards(dir.path()).unwrap();
        assert!(
            shards.len() >= 2,
            "expected multiple shards, got {}",
            shards.len()
        );
        for shard in &shards {
            let name = shard.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                name.starts_with("corpus_") && name.ends_with(".txt"),
                "{name}"
            );
            let content = std::fs::read_to_string(shard).unwrap();
            assert!(content.contains(DEFAULT_SEPARATOR));
            assert!(
                !content.starts_with('{'),
                "txt shard must not contain JSONL records"
            );
            assert!(
                !content.contains("# Document"),
                "markdown heading leaked into corpus"
            );
        }

        let manifest = crate::storage::load_manifest(dir.path(), "manifest.json")
            .unwrap()
            .expect("manifest written");
        assert_eq!(manifest.format, "txt");
        assert_eq!(manifest.separator, DEFAULT_SEPARATOR);
        assert_eq!(manifest.documents, 40);
        assert_eq!(manifest.shards.len(), shards.len());
        assert!(manifest.rejects.is_empty());
        assert!(manifest.shards.iter().all(|shard| shard.sha256.len() == 64));

        let report = verify_dataset(dir.path(), "manifest.json").unwrap();
        assert!(report.is_ok(), "failures: {:?}", report.failures);
        assert_eq!(report.documents, 40);
    }

    #[test]
    fn jsonl_format_still_writes_compressed_records() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &jsonl_config(), stats).unwrap();
        for index in 0..40 {
            writer.write(&document(index)).unwrap();
        }
        writer.finalize().unwrap();

        let shards = list_shards(dir.path()).unwrap();
        assert!(shards.len() >= 2);
        for shard in &shards {
            let name = shard.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.ends_with(".jsonl.zst"), "{name}");
        }
        let file = std::fs::File::open(&shards[0]).unwrap();
        let decoder = zstd::stream::read::Decoder::new(BufReader::new(file)).unwrap();
        let first = BufReader::new(decoder).lines().next().unwrap().unwrap();
        let parsed: StoredDocument = serde_json::from_str(&first).unwrap();
        assert!(!parsed.text.is_empty());

        let report = verify_dataset(dir.path(), "manifest.json").unwrap();
        assert!(report.is_ok(), "failures: {:?}", report.failures);
        assert_eq!(report.documents, 40);
    }

    #[test]
    fn topic_blocks_are_written_as_separate_records() {
        let dir = tempfile::tempdir().unwrap();
        let output = OutputConfig {
            max_chunk_words: 100,
            min_chunk_words: 5,
            ..OutputConfig::default()
        };
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &output, stats.clone()).unwrap();
        let text = "# Konu A\n\nKonu A hakkında yeterince uzun bir paragraf metni burada yer alıyor.\n\n## Konu B\n\nKonu B hakkında başka bir paragraf metni burada yer alıyor.";
        let document = StoredDocument {
            id: blake3::hash(text.as_bytes()).to_hex().to_string(),
            url: "https://example.com/topics".to_string(),
            domain: "example.com".to_string(),
            title: "Konu A".to_string(),
            language: "tr".to_string(),
            language_score: 0.99,
            word_count: text.split_whitespace().count(),
            char_count: text.chars().count(),
            simhash: "0x0123456789abcdef".to_string(),
            scraped_at: "2026-11-25T22:30:00Z".to_string(),
            text: text.to_string(),
        };
        writer.write(&document).unwrap();
        writer.finalize().unwrap();

        let manifest = crate::storage::load_manifest(dir.path(), "manifest.json")
            .unwrap()
            .unwrap();
        assert_eq!(manifest.documents, 2, "each topic block is a record");
        let shards = list_shards(dir.path()).unwrap();
        let content = std::fs::read_to_string(&shards[0]).unwrap();
        assert_eq!(content.matches(DEFAULT_SEPARATOR).count(), 2);
        assert!(content.contains("Konu B"));
        assert_eq!(stats.snapshot().records, 2);
        assert!(verify_dataset(dir.path(), "manifest.json").unwrap().is_ok());
    }

    #[test]
    fn repeated_paragraphs_are_written_once_across_documents() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let mut writer =
            ShardWriter::new(dir.path(), &OutputConfig::default(), stats.clone()).unwrap();
        let shared =
            "Bu paragraf iki ayrı sayfada aynen tekrarlanıyor ve korpusta bir kez kalmalıdır.";
        for (index, unique) in [
            "Birinci sayfaya özgü başka bir paragraf burada yer alıyor.",
            "İkinci sayfaya özgü farklı bir paragraf da burada yer alıyor.",
        ]
        .iter()
        .enumerate()
        {
            let text = format!("# Sayfa {index}\n\n{shared}\n\n{unique}");
            let document = StoredDocument {
                id: blake3::hash(text.as_bytes()).to_hex().to_string(),
                url: format!("https://example.com/page-{index}"),
                domain: "example.com".to_string(),
                title: format!("Sayfa {index}"),
                language: "tr".to_string(),
                language_score: 0.99,
                word_count: text.split_whitespace().count(),
                char_count: text.chars().count(),
                simhash: format!("0x{:016x}", index as u64 + 11),
                scraped_at: "2026-11-25T22:30:00Z".to_string(),
                text,
            };
            writer.write(&document).unwrap();
        }
        writer.finalize().unwrap();

        let shards = list_shards(dir.path()).unwrap();
        let content = std::fs::read_to_string(&shards[0]).unwrap();
        assert_eq!(
            content.matches(shared).count(),
            1,
            "shared paragraph written twice:\n{content}"
        );
        assert!(content.contains("Birinci sayfaya özgü"));
        assert!(content.contains("İkinci sayfaya özgü"));
        assert!(stats.snapshot().dedup_removed >= 1);
    }

    #[test]
    fn resume_continues_shard_numbering_and_keeps_manifest_history() {
        let dir = tempfile::tempdir().unwrap();
        let output = txt_config();
        let docs = |start: usize| document(start);
        {
            let stats = Arc::new(Stats::new());
            let mut writer = ShardWriter::new(dir.path(), &output, stats).unwrap();
            for index in 0..10 {
                writer.write(&docs(index)).unwrap();
            }
            writer.finalize().unwrap();
        }
        let first = crate::storage::load_manifest(dir.path(), "manifest.json")
            .unwrap()
            .unwrap();
        {
            let stats = Arc::new(Stats::new());
            let mut writer = ShardWriter::new(dir.path(), &output, stats).unwrap();
            for index in 10..20 {
                writer.write(&docs(index)).unwrap();
            }
            writer.finalize().unwrap();
        }
        let second = crate::storage::load_manifest(dir.path(), "manifest.json")
            .unwrap()
            .unwrap();
        assert_eq!(second.documents, first.documents + 10);
        assert!(verify_dataset(dir.path(), "manifest.json").unwrap().is_ok());
    }

    #[test]
    fn manifest_is_replaced_atomically_without_leftover_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &OutputConfig::default(), stats).unwrap();
        writer.write(&document(1)).unwrap();
        writer.finalize().unwrap();
        assert!(dir.path().join("manifest.json").exists());
        assert!(!dir.path().join("manifest.json.tmp").exists());
    }

    #[test]
    fn empty_shards_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &OutputConfig::default(), stats).unwrap();
        writer.finalize().unwrap();
        assert!(list_shards(dir.path()).unwrap().is_empty());
        let manifest = crate::storage::load_manifest(dir.path(), "manifest.json")
            .unwrap()
            .unwrap();
        assert_eq!(manifest.documents, 0);
    }

    #[test]
    fn shard_index_parsing_roundtrip() {
        assert_eq!(parse_shard_index("shard_00042.jsonl.zst"), Some(42));
        assert_eq!(parse_shard_index("shard_00042.jsonl"), None);
        assert_eq!(parse_shard_index("corpus_00007.txt"), Some(7));
        assert_eq!(parse_shard_index("notes.txt"), None);
    }
}
