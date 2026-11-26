//! Dataset storage: the record schema, plain-text and JSONL shard writing, the
//! atomic manifest and integrity verification.

pub mod shard;
pub mod urls;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Result, ScraperError};

pub use shard::{Manifest, ShardMeta, ShardWriter};
pub use urls::{is_content_page, SavedUrls, DEFAULT_SAVED_URLS_FILE};

/// One line of a `shard_XXXXX.jsonl.zst` file.
///
/// The field order is the on-disk column order and must not change without a
/// manifest version bump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Document {
    /// BLAKE3-256 of the final text, lowercase hex.
    pub id: String,
    /// Canonical source URL.
    pub url: String,
    /// Registrable domain of the URL.
    pub domain: String,
    /// Page title.
    pub title: String,
    /// ISO language code.
    pub language: String,
    /// Language detection confidence.
    pub language_score: f64,
    /// Word count of `text`.
    pub word_count: usize,
    /// Character count of `text`.
    pub char_count: usize,
    /// 64-bit SimHash as `0x` followed by 16 hex digits.
    pub simhash: String,
    /// RFC 3339 UTC timestamp of the fetch.
    pub scraped_at: String,
    /// Cleaned Markdown body.
    pub text: String,
}

/// Result of verifying a dataset directory.
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    /// Shards inspected.
    pub shards: usize,
    /// Documents parsed and schema-checked.
    pub documents: u64,
    /// Words found across all documents.
    pub words: u64,
    /// Long lines (>= 10 words) that occur in more than one document block.
    pub duplicate_lines: u64,
    /// Document blocks that are near duplicates of an earlier block.
    pub duplicate_blocks: u64,
    /// Human readable problems; empty means the dataset is intact.
    pub failures: Vec<String>,
}

impl VerifyReport {
    /// True when no problem was found.
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// SHA-256 of a file, streamed in 64 KiB chunks.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).map_err(|e| ScraperError::io_at(path, e))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut reader, &mut buffer)
            .map_err(|e| ScraperError::io_at(path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Load the manifest of an output directory, if it exists.
pub fn load_manifest(output_dir: &Path, manifest_name: &str) -> Result<Option<Manifest>> {
    let path = output_dir.join(manifest_name);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|e| ScraperError::io_at(&path, e))?;
    let manifest = serde_json::from_slice(&bytes).map_err(|error| {
        ScraperError::storage(format!("corrupt manifest {}: {error}", path.display()))
    })?;
    Ok(Some(manifest))
}

/// Verify every shard against the manifest: size, SHA-256, Zstd frame, JSON
/// schema, document counts and word totals.
pub fn verify_dataset(output_dir: &Path, manifest_name: &str) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let Some(manifest) = load_manifest(output_dir, manifest_name)? else {
        return Err(ScraperError::storage(format!(
            "no manifest named {manifest_name} in {}",
            output_dir.display()
        )));
    };

    let mut duplicates = DuplicateCheck::new();
    for shard in &manifest.shards {
        report.shards += 1;
        let path = output_dir.join(&shard.name);
        if !path.exists() {
            report
                .failures
                .push(format!("{}: file is missing", shard.name));
            continue;
        }
        let size = std::fs::metadata(&path)
            .map_err(|e| ScraperError::io_at(&path, e))?
            .len();
        if size != shard.size_bytes {
            report.failures.push(format!(
                "{}: size mismatch (manifest {}, disk {})",
                shard.name, shard.size_bytes, size
            ));
        }
        let digest = sha256_file(&path)?;
        if digest != shard.sha256 {
            report.failures.push(format!(
                "{}: sha256 mismatch (manifest {}, disk {})",
                shard.name, shard.sha256, digest
            ));
        }

        let (documents, words) = if shard.name.ends_with(".txt") {
            verify_txt_shard(
                &path,
                &shard.name,
                &manifest.separator,
                &mut report,
                &mut duplicates,
            )?
        } else {
            verify_jsonl_shard(&path, &shard.name, &mut report)?
        };
        report.documents += documents;
        report.words += words;
        if documents != shard.documents {
            report.failures.push(format!(
                "{}: document count mismatch (manifest {}, disk {})",
                shard.name, shard.documents, documents
            ));
        }
        if words != shard.words {
            report.failures.push(format!(
                "{}: word count mismatch (manifest {}, disk {})",
                shard.name, shard.words, words
            ));
        }
    }

    if report.documents != manifest.documents {
        report.failures.push(format!(
            "manifest documents {} != shard documents {}",
            manifest.documents, report.documents
        ));
    }
    if report.words != manifest.words {
        report.failures.push(format!(
            "manifest words {} != shard words {}",
            manifest.words, report.words
        ));
    }
    report.duplicate_lines = duplicates.duplicate_lines;
    report.duplicate_blocks = duplicates.duplicate_blocks;
    if report.duplicate_blocks > 0 {
        report.failures.push(format!(
            "{} near-duplicate document block(s); deduplication is not complete",
            report.duplicate_blocks
        ));
    }
    let line_budget = (report.documents / 100).max(1);
    if report.duplicate_lines > line_budget {
        report.failures.push(format!(
            "{} long line(s) repeat across documents (allowed: {})",
            report.duplicate_lines, line_budget
        ));
    }
    Ok(report)
}

/// Cross-document repetition detector used by `verify`.
///
/// It keeps a bounded set of long-line hashes and a 4-band SimHash index of
/// document blocks, so verification of a TB-scale corpus runs in constant
/// memory: both structures are cleared when they hit their cap.
struct DuplicateCheck {
    lines: HashMap<u64, ()>,
    blocks: HashMap<(u8, u16), Vec<u64>>,
    duplicate_lines: u64,
    duplicate_blocks: u64,
}

impl DuplicateCheck {
    const LINE_CAPACITY: usize = 2_000_000;
    const BLOCK_CAPACITY: usize = 200_000;
    const MIN_LINE_WORDS: usize = 10;
    const MIN_BLOCK_WORDS: usize = 40;
    const MAX_DISTANCE: u32 = 3;

    fn new() -> Self {
        Self {
            lines: HashMap::new(),
            blocks: HashMap::new(),
            duplicate_lines: 0,
            duplicate_blocks: 0,
        }
    }

    fn inspect(&mut self, block: &str) {
        for line in block.lines() {
            let line = line.trim();
            if line.split_whitespace().count() < Self::MIN_LINE_WORDS {
                continue;
            }
            let key =
                xxhash_rust::xxh3::xxh3_64(crate::dedup::exact::canonicalize(line).as_bytes());
            if self.lines.contains_key(&key) {
                self.duplicate_lines += 1;
                continue;
            }
            if self.lines.len() >= Self::LINE_CAPACITY {
                self.lines.clear();
            }
            self.lines.insert(key, ());
        }

        if block.split_whitespace().count() < Self::MIN_BLOCK_WORDS {
            return;
        }
        let fingerprint = crate::dedup::simhash::simhash64(block, 5);
        let bands = [
            (0u8, (fingerprint >> 48) as u16),
            (1u8, (fingerprint >> 32) as u16),
            (2u8, (fingerprint >> 16) as u16),
            (3u8, fingerprint as u16),
        ];
        for band in bands {
            if let Some(bucket) = self.blocks.get(&band) {
                if bucket.iter().any(|stored| {
                    crate::dedup::simhash::hamming_distance(*stored, fingerprint)
                        <= Self::MAX_DISTANCE
                }) {
                    self.duplicate_blocks += 1;
                    return;
                }
            }
        }
        if self.blocks.len() >= Self::BLOCK_CAPACITY {
            self.blocks.clear();
        }
        for band in bands {
            self.blocks.entry(band).or_default().push(fingerprint);
        }
    }
}

fn verify_txt_shard(
    path: &Path,
    name: &str,
    separator: &str,
    report: &mut VerifyReport,
    duplicates: &mut DuplicateCheck,
) -> Result<(u64, u64)> {
    let separator = if separator.trim().is_empty() {
        crate::export::DEFAULT_SEPARATOR
    } else {
        separator.trim()
    };
    let file = std::fs::File::open(path).map_err(|e| ScraperError::io_at(path, e))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut documents = 0u64;
    let mut words = 0u64;
    let mut block_words = 0u64;
    let mut block = String::new();
    let mut line_number = 0usize;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| ScraperError::io_at(path, e))?;
        if read == 0 {
            break;
        }
        line_number += 1;
        let text = String::from_utf8_lossy(&line);
        let trimmed = text.trim();
        if trimmed == separator {
            if block_words == 0 {
                report
                    .failures
                    .push(format!("{name}:{line_number}: empty document block"));
            }
            duplicates.inspect(&block);
            block.clear();
            documents += 1;
            words += block_words;
            block_words = 0;
        } else {
            block_words += trimmed.split_whitespace().count() as u64;
            if block.len() < 1024 * 1024 {
                block.push_str(&text);
            }
        }
    }
    if block_words > 0 {
        report
            .failures
            .push(format!("{name}: document without a trailing separator"));
        duplicates.inspect(&block);
        documents += 1;
        words += block_words;
    }
    Ok((documents, words))
}

fn verify_jsonl_shard(path: &Path, name: &str, report: &mut VerifyReport) -> Result<(u64, u64)> {
    let file = std::fs::File::open(path).map_err(|e| ScraperError::io_at(path, e))?;
    let decoder = match zstd::stream::read::Decoder::new(BufReader::new(file)) {
        Ok(decoder) => decoder,
        Err(error) => {
            report
                .failures
                .push(format!("{name}: cannot open zstd frame: {error}"));
            return Ok((0, 0));
        }
    };
    let reader = BufReader::new(decoder);
    let mut documents = 0u64;
    let mut words = 0u64;
    for (line_number, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                report.failures.push(format!(
                    "{name}:{}: cannot decompress line: {error}",
                    line_number + 1
                ));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Document>(&line) {
            Ok(document) => {
                documents += 1;
                words += document.word_count as u64;
                for problem in validate_document(&document) {
                    report
                        .failures
                        .push(format!("{name}:{}: {problem}", line_number + 1));
                }
            }
            Err(error) => report.failures.push(format!(
                "{name}:{}: invalid JSON schema: {error}",
                line_number + 1
            )),
        }
    }
    Ok((documents, words))
}

/// Validate a single record against the published schema.
pub fn validate_document(document: &Document) -> Vec<String> {
    let mut problems = Vec::new();
    if document.id.len() != 64 || !document.id.chars().all(|c| c.is_ascii_hexdigit()) {
        problems.push(format!("id is not a blake3 hex digest: {:?}", document.id));
    }
    if document.url.trim().is_empty() {
        problems.push("url is empty".to_string());
    }
    if document.domain.trim().is_empty() {
        problems.push("domain is empty".to_string());
    }
    if document.language.trim().is_empty() {
        problems.push("language is empty".to_string());
    }
    if !(0.0..=1.0).contains(&document.language_score) {
        problems.push(format!(
            "language_score out of range: {}",
            document.language_score
        ));
    }
    if document.word_count == 0 {
        problems.push("word_count is zero".to_string());
    }
    if document.char_count == 0 {
        problems.push("char_count is zero".to_string());
    }
    if document.text.trim().is_empty() {
        problems.push("text is empty".to_string());
    }
    if document.text.split_whitespace().count() != document.word_count {
        problems.push(format!(
            "word_count {} does not match text ({})",
            document.word_count,
            document.text.split_whitespace().count()
        ));
    }
    if document.text.chars().count() != document.char_count {
        problems.push(format!(
            "char_count {} does not match text ({})",
            document.char_count,
            document.text.chars().count()
        ));
    }
    if !document.simhash.starts_with("0x") || document.simhash.len() != 18 {
        problems.push(format!(
            "simhash is not 0x + 16 hex digits: {:?}",
            document.simhash
        ));
    } else if !u64::from_str_radix(&document.simhash[2..], 16).is_ok() {
        problems.push(format!(
            "simhash is not hexadecimal: {:?}",
            document.simhash
        ));
    }
    if chrono::DateTime::parse_from_rfc3339(&document.scraped_at).is_err() {
        problems.push(format!(
            "scraped_at is not RFC 3339: {:?}",
            document.scraped_at
        ));
    }
    problems
}

/// Dataset level statistics read from the manifest.
#[derive(Debug, Clone, Default)]
pub struct DatasetSummary {
    /// Number of shards listed.
    pub shards: u64,
    /// Documents listed.
    pub documents: u64,
    /// Words listed.
    pub words: u64,
    /// Uncompressed bytes listed.
    pub bytes_uncompressed: u64,
    /// Compressed bytes on disk.
    pub bytes_compressed: u64,
    /// Reject breakdown recorded in the manifest.
    pub rejects: Vec<(String, u64)>,
}

/// Read dataset totals from the manifest (all zeroes when it is missing).
pub fn dataset_summary(output_dir: &Path, manifest_name: &str) -> Result<DatasetSummary> {
    let Some(manifest) = load_manifest(output_dir, manifest_name)? else {
        return Ok(DatasetSummary::default());
    };
    Ok(DatasetSummary {
        shards: manifest.shards.len() as u64,
        documents: manifest.documents,
        words: manifest.words,
        bytes_uncompressed: manifest.bytes_uncompressed,
        bytes_compressed: manifest.shards.iter().map(|shard| shard.size_bytes).sum(),
        rejects: manifest.rejects.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_document() -> Document {
        let text = "# Title\n\nA body paragraph with a handful of useful words inside it.";
        Document {
            id: blake3::hash(text.as_bytes()).to_hex().to_string(),
            url: "https://example.com/page".to_string(),
            domain: "example.com".to_string(),
            title: "Title".to_string(),
            language: "en".to_string(),
            language_score: 0.99,
            word_count: text.split_whitespace().count(),
            char_count: text.chars().count(),
            simhash: "0x9f8e7d6c5b4a3928".to_string(),
            scraped_at: "2026-11-25T22:30:00Z".to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn schema_validation_accepts_a_well_formed_record() {
        assert!(validate_document(&sample_document()).is_empty());
    }

    #[test]
    fn schema_validation_flags_broken_records() {
        let mut document = sample_document();
        document.word_count = 999;
        document.simhash = "nope".to_string();
        document.scraped_at = "yesterday".to_string();
        document.id = "xyz".to_string();
        let problems = validate_document(&document);
        assert!(problems.iter().any(|p| p.contains("word_count")));
        assert!(problems.iter().any(|p| p.contains("simhash")));
        assert!(problems.iter().any(|p| p.contains("scraped_at")));
        assert!(problems.iter().any(|p| p.contains("blake3")));
    }

    #[test]
    fn duplicate_check_flags_repeated_blocks_and_lines() {
        let mut check = DuplicateCheck::new();
        let block = "Bu uzun cümle bir belge bloğunda yer alıyor ve on kelimeden fazla \
                     sözcük içeriyor. "
            .repeat(4);
        assert!(block.split_whitespace().count() >= DuplicateCheck::MIN_BLOCK_WORDS);
        check.inspect(&block);
        assert_eq!(check.duplicate_blocks, 0);
        assert_eq!(check.duplicate_lines, 0);
        check.inspect(&block);
        assert_eq!(check.duplicate_blocks, 1);
        assert_eq!(check.duplicate_lines, 1);
    }

    #[test]
    fn missing_manifest_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let error = verify_dataset(dir.path(), "manifest.json").expect_err("missing manifest");
        assert!(error.to_string().contains("no manifest"));
    }
}
