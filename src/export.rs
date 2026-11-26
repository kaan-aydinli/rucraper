//! Plain-text pretraining export.
//!
//! The shard format is a rich JSONL record per document, which is ideal for
//! auditing and deduplication but not what a training loader wants. This module
//! turns a verified dataset into one flat `.txt` corpus:
//!
//! * Markdown structure is stripped (heading hashes, list markers, fences,
//!   table rows, emphasis markers), so only prose remains;
//! * documents are separated by a configurable marker (`<veribitisi>`);
//! * the title is written only when it is actually about the text, which is
//!   decided by content-word overlap rather than by trusting the markup.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use xxhash_rust::xxh3::xxh3_64;

use crate::dedup::exact::canonicalize;
use crate::error::{Result, ScraperError};
use crate::storage::{load_manifest, Document};

/// Default delimiter between documents.
pub const DEFAULT_SEPARATOR: &str = "<veribitisi>";
/// Default cap for one topic block, in words.
pub const DEFAULT_MAX_CHUNK_WORDS: u64 = 3_000;
/// Blocks shorter than this are merged with the following section.
pub const DEFAULT_MIN_CHUNK_WORDS: u64 = 25;
/// Shortest sentence/paragraph that is checked for duplication.
pub const DEFAULT_DEDUP_MIN_SENTENCE_WORDS: usize = 6;
/// Hard cap of the corpus-wide duplicate index (constant memory).
pub const DEFAULT_DEDUP_INDEX_CAPACITY: usize = 500_000;

/// How many distinct title tokens are considered.
const MAX_TITLE_TOKENS: usize = 24;
/// How many body words are scanned when checking title relevance.
const MAX_BODY_SCAN_WORDS: usize = 20_000;
/// Minimum share of title content words that must occur in the body.
const TITLE_MATCH_RATIO: f64 = 0.34;

/// Function words that carry no topical meaning in titles.
const STOPWORDS: &[&str] = &[
    "ve", "ile", "için", "bir", "bu", "da", "de", "en", "çok", "gibi", "olan", "olarak", "ise",
    "ama", "fakat", "veya", "ya", "ki", "mi", "mu", "mı", "ne", "her", "daha", "sonra", "önce",
    "göre", "kadar", "ancak", "hem", "şey", "yani", "diye", "the", "and", "for", "with", "from",
    "that", "this", "are", "was", "were", "has", "have", "not", "but", "you", "your", "their",
    "its", "his", "her", "they", "them", "about", "into", "over", "under", "more", "most", "can",
    "will", "would", "should", "there", "here", "what", "when", "where", "which", "who", "all",
];

/// Export configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    /// Marker written on its own line after every document.
    pub separator: String,
    /// Write the title only when it shares content words with the body.
    pub include_relevant_title: bool,
    /// Largest topic block, in words; longer sections are split at paragraphs.
    pub max_chunk_words: u64,
    /// Blocks shorter than this are merged with the following section.
    pub min_chunk_words: u64,
    /// Remove paragraphs, sentences and whole blocks already written earlier.
    pub deduplicate: bool,
    /// Shortest sentence/paragraph checked for duplication.
    pub dedup_min_sentence_words: usize,
    /// Size bound of the duplicate index (keeps memory constant).
    pub dedup_index_capacity: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            separator: DEFAULT_SEPARATOR.to_string(),
            include_relevant_title: true,
            max_chunk_words: DEFAULT_MAX_CHUNK_WORDS,
            min_chunk_words: DEFAULT_MIN_CHUNK_WORDS,
            deduplicate: true,
            dedup_min_sentence_words: DEFAULT_DEDUP_MIN_SENTENCE_WORDS,
            dedup_index_capacity: DEFAULT_DEDUP_INDEX_CAPACITY,
        }
    }
}

/// How documents are turned into topic blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkOptions {
    /// Write the title only when it shares content words with the body.
    pub include_relevant_title: bool,
    /// Marker written on its own line after every block.
    pub separator: String,
    /// Largest topic block, in words.
    pub max_words: u64,
    /// Blocks shorter than this are merged with the following section.
    pub min_words: u64,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            include_relevant_title: true,
            separator: DEFAULT_SEPARATOR.to_string(),
            max_words: DEFAULT_MAX_CHUNK_WORDS,
            min_words: DEFAULT_MIN_CHUNK_WORDS,
        }
    }
}

/// One document rendered in plain-text pretraining form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattedDocument {
    /// Title (when relevant) plus body, terminated by the separator line.
    pub text: String,
    /// Words in the title+body block (the separator is not counted).
    pub words: u64,
    /// Characters in the title+body block.
    pub chars: u64,
    /// Whether the title was written.
    pub title_included: bool,
    /// Whether a title existed but was left out as unrelated.
    pub title_dropped: bool,
}

/// One coherent topic section of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TopicSection {
    heading: Option<String>,
    paragraphs: Vec<String>,
    words: u64,
}

impl TopicSection {
    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(heading) = &self.heading {
            out.push_str(heading);
        }
        for paragraph in &self.paragraphs {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(paragraph);
        }
        out
    }
}

/// Split a page into topic blocks and render each one for the corpus.
pub fn format_document_blocks(
    document: &Document,
    options: &ChunkOptions,
) -> Vec<FormattedDocument> {
    format_markdown_blocks(&document.title, &document.text, options)
}

/// Split cleaned Markdown into topic blocks and render each one.
///
/// Headings are the primary topic boundary; a section that exceeds
/// `max_words` is cut at paragraph (or sentence) boundaries, and a section
/// shorter than `min_words` is merged with the following one. Every returned
/// block is separated from the next by the configured marker, so unrelated
/// topics never share a training record.
///
/// The title is written **once per document**, on the first block that carries
/// content. Repeating the headline on every chunk was the largest source of
/// duplication in topic-split corpora: a 7-section page produced the same title
/// line 7 times in a row. Headings and paragraphs that merely repeat the title
/// are dropped as well.
pub fn format_markdown_blocks(
    title: &str,
    markdown: &str,
    options: &ChunkOptions,
) -> Vec<FormattedDocument> {
    let body = markdown_to_text(markdown);
    if body.trim().is_empty() {
        return Vec::new();
    }
    let had_title = !title.trim().is_empty();
    let title = clean_title(title);
    let title = title.trim();
    let include =
        options.include_relevant_title && !title.is_empty() && title_is_relevant(title, &body);
    let title_key = canonicalize(title);

    let mut sections = topic_sections(markdown, options.max_words, options.min_words);
    // Reference apparatus ("Ayrıca bakınız", "Notlar", footnote dumps) is not
    // prose and must never become training text.
    sections.retain(|section| {
        !section
            .heading
            .as_deref()
            .map(is_reference_heading)
            .unwrap_or(false)
    });
    for section in sections.iter_mut() {
        if let Some(heading) = section.heading.as_deref() {
            if !title_key.is_empty() && canonicalize(heading) == title_key {
                section.heading = None;
            }
        }
        section.paragraphs.retain(|paragraph| {
            !is_reference_paragraph(paragraph)
                && (title_key.is_empty() || canonicalize(paragraph) != title_key)
        });
        section.words = section
            .heading
            .as_ref()
            .map(|heading| heading.split_whitespace().count() as u64)
            .unwrap_or(0)
            + section
                .paragraphs
                .iter()
                .map(|paragraph| paragraph.split_whitespace().count() as u64)
                .sum::<u64>();
    }
    // A heading without any surviving paragraph is a stray navigation/title
    // fragment, not a topic: writing it would produce a seven-word record.
    sections.retain(|section| !section.paragraphs.is_empty());

    let mut blocks = Vec::with_capacity(sections.len());
    let mut title_written = false;
    for section in &sections {
        let content_text = section.render();
        if content_text.trim().is_empty() {
            continue;
        }
        let include_here = include && !title_written;
        let mut content = String::with_capacity(content_text.len() + title.len() + 16);
        if include_here {
            content.push_str(title);
            content.push_str("\n\n");
            title_written = true;
        }
        content.push_str(&content_text);

        let words = content.split_whitespace().count() as u64;
        let chars = content.chars().count() as u64;
        let mut text = content;
        text.push('\n');
        text.push_str(&options.separator);
        text.push_str("\n\n");

        let title_dropped = blocks.is_empty() && !include && had_title;
        blocks.push(FormattedDocument {
            text,
            words,
            chars,
            title_included: include_here,
            title_dropped,
        });
    }
    blocks
}

/// Corpus-wide duplicate remover for plain-text records.
///
/// Paragraphs, sentences and whole blocks are canonicalised (lowercase,
/// punctuation and spacing removed) and hashed. A repeat is dropped after its
/// first occurrence, which removes repeated headlines, lead paragraphs replayed
/// by the article body, quoted tweets transcribed twice and republished text.
///
/// The index is a fixed-capacity hash set, so a TB-scale corpus runs in constant
/// memory: reaching the cap clears the set and deduplication restarts on the
/// next window instead of growing without bound.
#[derive(Debug)]
pub struct CorpusDeduper {
    seen: HashSet<u64>,
    capacity: usize,
    min_sentence_words: usize,
    removed_paragraphs: u64,
    removed_sentences: u64,
    removed_blocks: u64,
}

impl CorpusDeduper {
    /// Create a deduper with a hard memory bound.
    pub fn new(capacity: usize, min_sentence_words: usize) -> Self {
        Self {
            seen: HashSet::new(),
            capacity: capacity.max(1024),
            min_sentence_words: min_sentence_words.max(3),
            removed_paragraphs: 0,
            removed_sentences: 0,
            removed_blocks: 0,
        }
    }

    /// Total units (blocks, paragraphs, sentences) removed so far.
    pub fn removed(&self) -> u64 {
        self.removed_blocks + self.removed_paragraphs + self.removed_sentences
    }

    /// Paragraphs removed so far.
    pub fn removed_paragraphs(&self) -> u64 {
        self.removed_paragraphs
    }

    /// Sentences removed so far.
    pub fn removed_sentences(&self) -> u64 {
        self.removed_sentences
    }

    /// Whole blocks removed so far.
    pub fn removed_blocks(&self) -> u64 {
        self.removed_blocks
    }

    /// Number of hashes currently indexed (tests/diagnostics).
    pub fn tracked(&self) -> usize {
        self.seen.len()
    }

    fn remember(&mut self, text: &str) -> bool {
        let key = xxh3_64(canonicalize(text).as_bytes());
        if self.seen.contains(&key) {
            return false;
        }
        if self.seen.len() >= self.capacity {
            self.seen.clear();
        }
        self.seen.insert(key);
        true
    }

    /// Drop paragraphs and sentences already seen; returns `None` when nothing
    /// of the paragraph survives.
    fn clean_paragraph(&mut self, paragraph: &str) -> Option<String> {
        // Plain-text shards can still carry bibliography lines, bare links and
        // navigation labels that the Markdown pass never saw; strip those per
        // line so `export-txt` also cleans an existing txt dataset.
        let filtered = paragraph
            .lines()
            .filter(|line| !is_noise_line(line))
            .collect::<Vec<_>>()
            .join("\n");
        let paragraph = filtered.trim();
        if paragraph.is_empty() {
            return None;
        }
        if paragraph.split_whitespace().count() < self.min_sentence_words {
            return Some(paragraph.to_string());
        }
        if !self.remember(paragraph) {
            self.removed_paragraphs += 1;
            return None;
        }
        let sentences = split_sentences(paragraph);
        if sentences.len() < 2 {
            return Some(paragraph.to_string());
        }
        let mut kept: Vec<&str> = Vec::with_capacity(sentences.len());
        for sentence in sentences {
            if sentence.split_whitespace().count() >= self.min_sentence_words
                && !self.remember(sentence)
            {
                self.removed_sentences += 1;
                continue;
            }
            kept.push(sentence);
        }
        if kept.is_empty() {
            self.removed_paragraphs += 1;
            return None;
        }
        Some(kept.join(" "))
    }

    /// Clean a Markdown body before it is split into topic blocks.
    pub fn clean_markdown(&mut self, markdown: &str) -> String {
        let mut out = String::with_capacity(markdown.len());
        for block in markdown.split("\n\n") {
            let trimmed = block.trim();
            if trimmed.is_empty() {
                continue;
            }
            let cleaned = if trimmed.starts_with('#') {
                Some(trimmed.to_string())
            } else {
                self.clean_paragraph(trimmed)
            };
            if let Some(cleaned) = cleaned {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&cleaned);
            }
        }
        out
    }

    /// Clean one rendered plain-text block; `None` when every paragraph in it
    /// is a duplicate of text written earlier.
    pub fn clean_block(&mut self, block: &str) -> Option<String> {
        let block = block.trim();
        if block.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(block.len());
        let mut kept_any = false;
        for paragraph in block.split("\n\n") {
            if let Some(cleaned) = self.clean_paragraph(paragraph) {
                kept_any = true;
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(&cleaned);
            }
        }
        if !kept_any {
            self.removed_blocks += 1;
            return None;
        }
        Some(out)
    }
}

impl Default for CorpusDeduper {
    fn default() -> Self {
        Self::new(
            DEFAULT_DEDUP_INDEX_CAPACITY,
            DEFAULT_DEDUP_MIN_SENTENCE_WORDS,
        )
    }
}

/// Split a paragraph into trimmed sentences, keeping their punctuation.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if !matches!(ch, '.' | '!' | '?' | '…') {
            continue;
        }
        let next_is_space = chars
            .peek()
            .map(|(_, next)| next.is_whitespace())
            .unwrap_or(true);
        if !next_is_space {
            continue;
        }
        let sentence = text[start..index + ch.len_utf8()].trim();
        if !sentence.is_empty() {
            sentences.push(sentence);
        }
        start = index + ch.len_utf8();
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        sentences.push(rest);
    }
    sentences
}

/// Parse Markdown into heading-delimited sections, then bound each one's size.
fn topic_sections(markdown: &str, max_words: u64, min_words: u64) -> Vec<TopicSection> {
    let max_words = max_words.max(1);
    let raw = split_sections(markdown);
    let mut pieces: Vec<TopicSection> = Vec::new();

    for (heading, body) in raw {
        let heading = heading
            .map(|text| strip_emphasis(&text).trim().to_string())
            .filter(|text| !text.is_empty());
        // Drop bibliography/see-also sections before packing: a short `Notlar`
        // section would otherwise merge into the next real section, and the
        // reference-heading filter would then discard genuine prose with it.
        if heading
            .as_deref()
            .map(is_reference_heading)
            .unwrap_or(false)
        {
            continue;
        }
        let plain = markdown_to_text(&body);
        let mut paragraphs: Vec<String> = Vec::new();
        for paragraph in plain.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
            if paragraph.split_whitespace().count() as u64 > max_words {
                paragraphs.extend(split_long_paragraph(paragraph, max_words));
            } else {
                paragraphs.push(paragraph.to_string());
            }
        }
        if paragraphs.is_empty() && heading.is_none() {
            continue;
        }

        let mut group: Vec<String> = Vec::new();
        let mut group_words = heading
            .as_ref()
            .map(|h| h.split_whitespace().count() as u64)
            .unwrap_or(0);
        let mut group_heading = heading;
        for paragraph in paragraphs {
            let words = paragraph.split_whitespace().count() as u64;
            if !group.is_empty() && group_words + words > max_words {
                pieces.push(TopicSection {
                    heading: group_heading.take(),
                    paragraphs: std::mem::take(&mut group),
                    words: group_words,
                });
                group_words = 0;
            }
            group.push(paragraph);
            group_words += words;
        }
        if !group.is_empty() || group_heading.is_some() {
            pieces.push(TopicSection {
                heading: group_heading,
                paragraphs: group,
                words: group_words,
            });
        }
    }

    // Avoid emitting fragments: a short section joins the next one when both
    // fit inside the size cap.
    let mut merged: Vec<TopicSection> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        if let Some(last) = merged.last_mut() {
            if last.words < min_words && last.words + piece.words <= max_words {
                if let Some(heading) = piece.heading {
                    last.paragraphs.push(heading);
                }
                last.paragraphs.extend(piece.paragraphs);
                last.words += piece.words;
                continue;
            }
        }
        merged.push(piece);
    }

    // The final piece has no successor to merge into, so fold a short tail
    // back into the previous section instead of emitting a fragment.
    if merged.len() >= 2 {
        let tail_words = merged.last().map(|piece| piece.words).unwrap_or(0);
        let needs_merge = tail_words < min_words;
        let fits = merged
            .iter()
            .rev()
            .nth(1)
            .map(|previous| previous.words + tail_words <= max_words)
            .unwrap_or(false);
        if needs_merge && fits {
            let tail = merged.pop().expect("tail exists");
            let previous = merged.last_mut().expect("previous exists");
            if let Some(heading) = tail.heading {
                previous.paragraphs.push(heading);
            }
            previous.paragraphs.extend(tail.paragraphs);
            previous.words += tail.words;
        }
    }
    merged
}

/// Split Markdown into `(heading, body)` pairs at ATX headings.
fn split_sections(markdown: &str) -> Vec<(Option<String>, String)> {
    let mut sections: Vec<(Option<String>, String)> = Vec::new();
    let mut heading: Option<String> = None;
    let mut body = String::new();
    let mut in_fence = false;

    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            body.push_str(line);
            body.push('\n');
            continue;
        }
        if !in_fence {
            if let Some(text) = parse_heading(trimmed) {
                flush_section(&mut heading, &mut body, &mut sections);
                heading = Some(text);
                continue;
            }
        }
        body.push_str(line);
        body.push('\n');
    }
    flush_section(&mut heading, &mut body, &mut sections);
    sections
}

fn flush_section(
    heading: &mut Option<String>,
    body: &mut String,
    out: &mut Vec<(Option<String>, String)>,
) {
    let taken_heading = heading.take();
    let taken_body = std::mem::take(body);
    if taken_heading.is_some() || !taken_body.trim().is_empty() {
        out.push((taken_heading, taken_body));
    }
}

fn parse_heading(line: &str) -> Option<String> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// Cut an over-long paragraph at sentence boundaries, staying under `max_words`.
fn split_long_paragraph(paragraph: &str, max_words: u64) -> Vec<String> {
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = paragraph.chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        let ends_sentence = matches!(ch, '.' | '!' | '?' | '…')
            && chars
                .peek()
                .map(|next| next.is_whitespace())
                .unwrap_or(true);
        if !ends_sentence {
            continue;
        }
        sentences.push(current.trim().to_string());
        current.clear();
        while matches!(chars.peek(), Some(next) if next.is_whitespace()) {
            chars.next();
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }

    let max_words = usize::try_from(max_words).unwrap_or(usize::MAX).max(1);
    let mut groups: Vec<String> = Vec::new();
    let mut group = String::new();
    let mut group_words = 0u64;
    for sentence in sentences {
        let words = sentence.split_whitespace().count() as u64;
        if words > max_words as u64 {
            if !group.trim().is_empty() {
                groups.push(std::mem::take(&mut group));
                group_words = 0;
            }
            let tokens: Vec<&str> = sentence.split_whitespace().collect();
            for chunk in tokens.chunks(max_words) {
                groups.push(chunk.join(" "));
            }
            continue;
        }
        if !group.is_empty() && group_words + words > max_words as u64 {
            groups.push(std::mem::take(&mut group));
            group_words = 0;
        }
        if !group.is_empty() {
            group.push(' ');
        }
        group.push_str(&sentence);
        group_words += words;
    }
    if !group.trim().is_empty() {
        groups.push(group);
    }
    groups
}

/// What an export produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// Documents written.
    pub documents: u64,
    /// Words written (body only).
    pub words: u64,
    /// Size of the final file in bytes.
    pub bytes: u64,
    /// Shards read.
    pub shards: usize,
    /// Titles that were relevant and written.
    pub titles_included: u64,
    /// Titles that were skipped as unrelated.
    pub titles_dropped: u64,
    /// Duplicate blocks, paragraphs and sentences removed.
    pub duplicate_units_removed: u64,
}

/// Export every shard of `dataset_dir` into one plain-text file.
pub fn export_txt(
    dataset_dir: &Path,
    manifest_name: &str,
    output: &Path,
    options: &ExportOptions,
) -> Result<ExportReport> {
    if options.separator.trim().is_empty() {
        return Err(ScraperError::config("export separator must not be empty"));
    }
    let manifest = load_manifest(dataset_dir, manifest_name)?.ok_or_else(|| {
        ScraperError::storage(format!(
            "no manifest named {manifest_name} in {}",
            dataset_dir.display()
        ))
    })?;

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| ScraperError::io_at(parent, e))?;
        }
    }
    let chunk_options = ChunkOptions {
        include_relevant_title: options.include_relevant_title,
        separator: options.separator.clone(),
        max_words: options.max_chunk_words,
        min_words: options.min_chunk_words,
    };
    let tmp_path = output.with_extension("txt.tmp");
    let file = std::fs::File::create(&tmp_path).map_err(|e| ScraperError::io_at(&tmp_path, e))?;
    let mut writer = BufWriter::new(file);
    let mut report = ExportReport {
        shards: manifest.shards.len(),
        ..ExportReport::default()
    };
    let mut deduper = options.deduplicate.then(|| {
        CorpusDeduper::new(
            options.dedup_index_capacity,
            options.dedup_min_sentence_words,
        )
    });

    for shard in &manifest.shards {
        let path = dataset_dir.join(&shard.name);

        // Plain-text shards are already in the target format: re-emit their
        // blocks so the requested separator is used and counts stay exact.
        if shard.name.ends_with(".txt") {
            let source_separator = if manifest.separator.trim().is_empty() {
                options.separator.clone()
            } else {
                manifest.separator.clone()
            };
            let content =
                std::fs::read_to_string(&path).map_err(|e| ScraperError::io_at(&path, e))?;
            for block in content.split(&source_separator) {
                let block = block.trim();
                if block.is_empty() {
                    continue;
                }
                let cleaned = match deduper.as_mut() {
                    Some(deduper) => match deduper.clean_block(block) {
                        Some(cleaned) => cleaned,
                        None => continue,
                    },
                    None => block.to_string(),
                };
                writer
                    .write_all(cleaned.as_bytes())
                    .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
                writer
                    .write_all(b"\n")
                    .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
                writer
                    .write_all(options.separator.as_bytes())
                    .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
                writer
                    .write_all(b"\n\n")
                    .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
                report.documents += 1;
                report.words += cleaned.split_whitespace().count() as u64;
            }
            continue;
        }

        let file = std::fs::File::open(&path).map_err(|e| ScraperError::io_at(&path, e))?;
        let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
            .map_err(|error| ScraperError::Zstd(format!("{}: {error}", path.display())))?;
        for (line_number, line) in BufReader::new(decoder).lines().enumerate() {
            let line = line.map_err(|error| {
                ScraperError::storage(format!(
                    "{}:{}: cannot read shard: {error}",
                    shard.name,
                    line_number + 1
                ))
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let document: Document = serde_json::from_str(&line).map_err(|error| {
                ScraperError::storage(format!(
                    "{}:{}: invalid record: {error}",
                    shard.name,
                    line_number + 1
                ))
            })?;
            write_document(
                &mut writer,
                &document,
                &chunk_options,
                &mut report,
                deduper.as_mut(),
            )?;
        }
    }

    writer
        .flush()
        .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
    let file = writer
        .into_inner()
        .map_err(|error| ScraperError::storage(format!("cannot flush output: {error}")))?;
    file.sync_all()
        .map_err(|e| ScraperError::io_at(&tmp_path, e))?;
    drop(file);
    std::fs::rename(&tmp_path, output).map_err(|e| ScraperError::io_at(output, e))?;

    if let Some(deduper) = &deduper {
        report.duplicate_units_removed = deduper.removed();
    }
    report.bytes = std::fs::metadata(output)
        .map_err(|e| ScraperError::io_at(output, e))?
        .len();
    Ok(report)
}

fn write_document<W: Write>(
    writer: &mut W,
    document: &Document,
    options: &ChunkOptions,
    report: &mut ExportReport,
    deduper: Option<&mut CorpusDeduper>,
) -> Result<()> {
    let cleaned = deduper.map(|deduper| deduper.clean_markdown(&document.text));
    let markdown = cleaned.as_deref().unwrap_or(&document.text);
    for formatted in format_markdown_blocks(&document.title, markdown, options) {
        if formatted.title_included {
            report.titles_included += 1;
        }
        if formatted.title_dropped {
            report.titles_dropped += 1;
        }
        writer
            .write_all(formatted.text.as_bytes())
            .map_err(|e| ScraperError::io_at("<export>", e))?;
        report.documents += 1;
        report.words += formatted.words;
    }
    Ok(())
}

/// Clean a page title before it is stored or rendered.
///
/// Site names are commonly appended after a separator (`Başlık | Site Adı`),
/// which used to leak "Ana Sayfa | Fırat Üniversitesi" into the corpus. Trailing
/// short segments without digits are dropped, and a title that is itself page
/// furniture (`Devamını Oku`, `Son Dakika`) becomes empty so only the body is
/// kept.
pub fn clean_title(title: &str) -> String {
    const SEPARATORS: &[&str] = &[" | ", " - ", " – ", " — ", " » ", " :: "];
    let mut head = title.trim();
    loop {
        let mut stripped = false;
        for separator in SEPARATORS {
            if let Some((before, after)) = head.rsplit_once(separator) {
                let tail = after.trim();
                let tail_words = tail.split_whitespace().count();
                if !tail.is_empty()
                    && tail_words <= 3
                    && !tail.chars().any(|c| c.is_ascii_digit())
                    && !before.trim().is_empty()
                {
                    head = before.trim();
                    stripped = true;
                    break;
                }
            }
        }
        if !stripped {
            break;
        }
    }
    if head.is_empty() || is_noise_line(head) {
        return String::new();
    }
    head.to_string()
}

/// Strip page furniture from an extracted Markdown body before it is stored.
///
/// This is the single cleaning pass the crawler applies while it runs, so both
/// output formats (`txt` and `jsonl`) hold the same cleaned text and the quality
/// filters measure what is actually saved. Bibliography lines, bare links,
/// navigation labels and inline numeric citations are removed; headings, lists
/// and code fences are preserved because they carry topic structure.
pub fn sanitize_markdown(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    // A `Twitter hesaplarımız:` label is followed by one bare handle per line;
    // drop those tokens until real prose starts again.
    let mut skipping_social_tokens = false;
    for raw_line in markdown.lines() {
        let trimmed = raw_line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(raw_line);
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push_str(raw_line);
            out.push('\n');
            continue;
        }
        if raw_line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        if skipping_social_tokens {
            if is_social_token(raw_line) {
                continue;
            }
            skipping_social_tokens = false;
        }
        if is_social_header(raw_line) {
            skipping_social_tokens = true;
            continue;
        }
        if is_noise_line(raw_line) {
            continue;
        }
        let cleaned = strip_inline_citations(raw_line);
        let cleaned = cleaned.trim_end();
        if cleaned.trim().is_empty() {
            continue;
        }
        out.push_str(cleaned);
        out.push('\n');
    }
    collapse_blank_lines(&out)
}

/// Convert the extracted Markdown body into plain prose.
pub fn markdown_to_text(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    let mut skip_short_paragraph = false;

    for raw_line in markdown.lines() {
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            if !trimmed.is_empty() {
                out.push_str(trimmed);
                out.push('\n');
            }
            continue;
        }
        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }
        if matches!(trimmed, "---" | "***" | "___") {
            continue;
        }
        if is_reference_marker(trimmed) {
            continue;
        }
        // Rendered table rows are data grids, not prose.
        if trimmed.contains(" | ") {
            continue;
        }

        let mut text = trimmed.trim_start_matches('#').trim_start();
        while let Some(rest) = text.strip_prefix('>') {
            text = rest.trim_start();
        }
        if let Some(rest) = text
            .strip_prefix("- ")
            .or_else(|| text.strip_prefix("* "))
            .or_else(|| text.strip_prefix("+ "))
        {
            text = rest.trim_start();
        }
        let cleaned = strip_emphasis(text);
        let cleaned = strip_inline_citations(&cleaned);
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            out.push('\n');
            continue;
        }
        // Metadata labels ("Haber Yeri", "Etiketler", ...) and the short value
        // paragraph that follows them are page furniture, not prose.
        if is_noise_line(cleaned) {
            continue;
        }
        if is_metadata_label(cleaned) {
            skip_short_paragraph = true;
            continue;
        }
        if skip_short_paragraph {
            skip_short_paragraph = false;
            if cleaned.split_whitespace().count() <= 3 {
                continue;
            }
        }
        // Standalone publication dates and tweet attribution lines.
        if is_standalone_date(cleaned) {
            continue;
        }
        out.push_str(cleaned.trim_end());
        out.push('\n');
    }

    collapse_blank_lines(&out)
}

/// Citation remnants such as `(AEK)`, `(HA)` or `[12]` that stand alone on a line.
fn is_reference_marker(line: &str) -> bool {
    let trimmed = line.trim();
    let inner = match (trimmed.strip_prefix('('), trimmed.strip_suffix(')')) {
        (Some(rest), Some(_)) => rest.strip_suffix(')').unwrap_or(rest),
        _ => match (trimmed.strip_prefix('['), trimmed.strip_suffix(']')) {
            (Some(rest), Some(_)) => rest.strip_suffix(']').unwrap_or(rest),
            _ => return false,
        },
    };
    let inner = inner.trim();
    !inner.is_empty()
        && inner.chars().count() <= 12
        && !inner.contains(['.', '!', '?', ',', ';', ':'])
}

/// One line of page furniture rather than prose.
///
/// Three independent families are caught here: bibliography citations that
/// survive a missing `Kaynakça` heading (Wikipedia footnote dumps are the worst
/// offender), bare links or social posts that carry no prose, and navigation /
/// legal labels that sit outside the article body.
fn is_noise_line(line: &str) -> bool {
    let text = strip_line_prefix(line);
    !text.is_empty() && (is_reference_line(text) || is_link_line(text) || is_furniture_line(text))
}

/// Remove blockquote, heading and list prefixes so the detectors see the real
/// content. `## Devamını Oku` is the same page furniture as `Devamını Oku`.
fn strip_line_prefix(line: &str) -> &str {
    let mut text = line.trim();
    loop {
        let before = text;
        while let Some(rest) = text.strip_prefix('>') {
            text = rest.trim_start();
        }
        while let Some(rest) = text.strip_prefix('#') {
            text = rest.trim_start();
        }
        if text == before {
            break;
        }
    }
    if let Some(rest) = text
        .strip_prefix("- ")
        .or_else(|| text.strip_prefix("* "))
        .or_else(|| text.strip_prefix("+ "))
    {
        text = rest.trim_start();
    }
    text
}

/// A social-media label whose following lines are bare handles or hashtags.
fn is_social_header(line: &str) -> bool {
    let text = strip_line_prefix(line).to_lowercase();
    const NEEDLES: &[&str] = &[
        "twitter hesaplar",
        "twitter hesab",
        "sosyal medya hesaplar",
        "bizi takip edin",
        "bizi takip et",
        "bizi sosyal medyadan takip",
    ];
    NEEDLES.iter().any(|needle| text.contains(needle))
}

/// A bare handle/hashtag line (`zkuşağı`, `#gündem`, `AKP`), the payload of a
/// social-media label, not a sentence.
fn is_social_token(line: &str) -> bool {
    let text = strip_line_prefix(line);
    if text.is_empty() || matches!(text.chars().last(), Some('.' | '!' | '?' | '…' | ',' | ';')) {
        return false;
    }
    text.split_whitespace().count() <= 2 && text.chars().count() <= 48
}

/// A bibliography/citation line: `↑ ...`, `Erişim tarihi: ...`, `ISBN ...`.
fn is_reference_line(line: &str) -> bool {
    if line.starts_with('↑') || line.starts_with('^') {
        return true;
    }
    const NEEDLES: &[&str] = &[
        "arşivlendi",
        "wayback machine",
        "erişim tarihi",
        "arşivlenme tarihi",
        "arşiv tarihi",
        "isbn",
        "doi:",
        "oclc",
        "pmid",
        "ss. ",
        "son erişim",
        "archived from the original",
    ];
    let lowered = line.to_lowercase();
    NEEDLES.iter().any(|needle| lowered.contains(needle))
}

/// A line that is a bare URL, a social post or a short caption ending in one.
fn is_link_line(line: &str) -> bool {
    const SOCIAL: &[&str] = &[
        "t.co/",
        "twitter.com/",
        "x.com/",
        "pic.twitter.com/",
        "instagram.com/",
        "facebook.com/",
        "youtube.com/watch",
        "youtu.be/",
        "bit.ly/",
        "goo.gl/",
        "t.me/",
    ];
    let lowered = line.to_lowercase();
    if SOCIAL.iter().any(|needle| lowered.contains(needle)) {
        return true;
    }
    // A citation or caption often ends with its source URL; that tail is not
    // prose no matter how long the sentence before it was.
    if let Some(last) = line.split_whitespace().last() {
        let last = last.trim_end_matches(['.', ',', ';', ')', ']', '"', '\'', '…']);
        if last.starts_with("http://") || last.starts_with("https://") {
            return true;
        }
    }
    let mut words = 0usize;
    let mut links = 0usize;
    for token in line.split_whitespace() {
        words += 1;
        let token = token.trim_matches(|c: char| c.is_ascii_punctuation() && c != '/' && c != ':');
        if token.starts_with("http://")
            || token.starts_with("https://")
            || token.starts_with("www.")
        {
            links += 1;
        }
    }
    links > 0 && (words <= 20 || links * 4 >= words)
}

/// Navigation, legal and call-to-action labels that are not article prose.
fn is_furniture_line(line: &str) -> bool {
    const LABELS: &[&str] = &[
        "anasayfa",
        "ana sayfa",
        "son dakika",
        "son dakika haberleri",
        "iletişim",
        "iletişim bilgileri",
        "hakkımızda",
        "hakkinda",
        "reklam",
        "reklamlar",
        "gizlilik",
        "gizlilik politikası",
        "gizlilik politikasi",
        "çerez politikası",
        "çerezler",
        "kişisel verilerin korunması",
        "kvkk",
        "künye",
        "yazarlar",
        "arşiv",
        "rss",
        "bizi takip edin",
        "bizi takip et",
        "twitter hesaplarımız",
        "twitter hesabımız",
        "sosyal medya",
        "sosyal medya hesaplarımız",
        "takip et",
        "abone ol",
        "abonelik",
        "bülten",
        "e-bülten",
        "üye ol",
        "giriş yap",
        "oturum aç",
        "çıkış yap",
        "menü",
        "arama",
        "aramaya devam et",
        "yorum yap",
        "yorumlar",
        "paylaş",
        "yazdır",
        "devamını oku",
        "devamini oku",
        "haberin devamı",
        "ilgili haberler",
        "ilgili içerikler",
        "öne çıkanlar",
        "en çok okunanlar",
        "popüler haberler",
        "editörün seçtikleri",
        "sizin için seçtiklerimiz",
        "bu haberle ilgili",
        "diğer haberler",
        "son haberler",
        "tüm haberler",
        "home",
        "homepage",
        "contact",
        "contact us",
        "about us",
        "privacy policy",
        "cookie policy",
        "terms of use",
        "advertisement",
        "subscribe",
        "newsletter",
        "sign in",
        "log in",
        "menu",
        "search",
        "share",
        "print",
        "read more",
        "related articles",
        "most read",
        "trending",
    ];
    let normalized = line
        .trim()
        .trim_end_matches(':')
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return false;
    }
    if LABELS.contains(&normalized.as_str()) {
        return true;
    }
    let lowered = line.to_lowercase();
    lowered.contains('©')
        || lowered.contains("tüm hakları saklıdır")
        || lowered.contains("all rights reserved")
}

/// Headings whose whole section is bibliography, see-also links or footnotes.
fn is_reference_heading(heading: &str) -> bool {
    const HEADINGS: &[&str] = &[
        "ayrıca bakınız",
        "ayrica bakiniz",
        "kaynakça",
        "kaynakca",
        "kaynaklar",
        "notlar",
        "dış bağlantılar",
        "dis baglantilar",
        "referanslar",
        "bibliyografya",
        "see also",
        "references",
        "external links",
        "bibliography",
        "notes",
        "further reading",
        "sources",
    ];
    let normalized = heading.trim().trim_end_matches(':').to_lowercase();
    HEADINGS.contains(&normalized.as_str())
}

/// True when a paragraph is nothing but footnote/reference markers.
fn is_reference_paragraph(paragraph: &str) -> bool {
    fn is_marker(line: &str) -> bool {
        let lower = line.to_lowercase();
        line.starts_with('↑')
            || line.starts_with('^')
            || lower.starts_with("isbn")
            || lower.starts_with("doi:")
            || lower.starts_with("oclc")
    }
    let mut lines = paragraph
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return false;
    };
    is_marker(first) && lines.all(is_marker)
}

/// True for lines that only announce metadata ("Haber Yeri", "Etiketler").
fn is_metadata_label(line: &str) -> bool {
    const LABELS: &[&str] = &[
        "haber yeri",
        "haber kaynagi",
        "haber kaynağı",
        "kaynak",
        "etiket",
        "etiketler",
        "tags",
        "paylaş",
        "paylaşım",
        "yorumlar",
        "okuma süresi",
        "güncelleme",
        "son güncelleme",
        "published",
        "updated",
        "yazar",
        "author",
    ];
    let normalized = line
        .trim()
        .trim_end_matches(':')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    LABELS.contains(&normalized.as_str())
}

/// True for lines that are nothing but a publication date ("21 Haziran 2017").
fn is_standalone_date(line: &str) -> bool {
    const MONTHS: &[&str] = &[
        "ocak",
        "şubat",
        "mart",
        "nisan",
        "mayıs",
        "haziran",
        "temmuz",
        "ağustos",
        "eylül",
        "ekim",
        "kasım",
        "aralık",
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
        "jan",
        "feb",
        "mar",
        "apr",
        "jun",
        "jul",
        "aug",
        "sep",
        "sept",
        "oct",
        "nov",
        "dec",
    ];
    let tokens: Vec<String> = line
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() || tokens.len() > 10 {
        return false;
    }
    let mut has_month = false;
    let mut has_year = false;
    for token in &tokens {
        if MONTHS.contains(&token.as_str()) {
            has_month = true;
            continue;
        }
        for part in token
            .split(|c: char| !c.is_ascii_digit())
            .filter(|part| !part.is_empty())
        {
            if part.len() == 4 && (1900..=2100).contains(&part.parse::<u32>().unwrap_or(0)) {
                has_year = true;
            }
        }
    }
    has_month && has_year
}

/// Remove inline numeric citations such as `[6]`, `[ 6 ]` or `[6, 7]`.
///
/// Wikipedia and news pages embed these inside prose; they carry no language
/// signal and break sentences apart, so they are dropped together with the
/// space in front of them.
fn strip_inline_citations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if ch == '[' {
            if let Some(offset) = text[index + 1..].find(']') {
                let inner = &text[index + 1..index + 1 + offset];
                let numeric = !inner.is_empty()
                    && inner.chars().count() <= 8
                    && inner.chars().any(|c| c.is_ascii_digit())
                    && inner
                        .chars()
                        .all(|c| c.is_ascii_digit() || c.is_whitespace() || c == ',');
                if numeric {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    index += 1 + offset + 1;
                    continue;
                }
            }
        }
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn strip_emphasis(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '`' => {}
            '*' => {
                while matches!(chars.peek(), Some('*')) {
                    chars.next();
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Collapse trailing whitespace and runs of blank lines into single blank lines.
fn collapse_blank_lines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0usize;
    for line in input.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Content words of `text`: lowercase, alphanumeric, at least three characters.
fn content_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() >= 3)
        .filter(|token| !STOPWORDS.contains(token))
        .map(str::to_string)
        .collect()
}

/// True when the title shares enough content words with the body to be about it.
pub fn title_is_relevant(title: &str, body: &str) -> bool {
    let mut significant = content_words(title);
    significant.truncate(MAX_TITLE_TOKENS);
    significant.sort();
    significant.dedup();
    if significant.is_empty() {
        return false;
    }
    let mut body_words: HashSet<String> = HashSet::with_capacity(4096);
    for word in body.split_whitespace().take(MAX_BODY_SCAN_WORDS) {
        for token in content_words(word) {
            body_words.insert(token);
        }
    }
    let matched = significant
        .iter()
        .filter(|token| body_words.contains(*token))
        .count();
    matched > 0 && (matched as f64 / significant.len() as f64) >= TITLE_MATCH_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::OutputConfig;
    use crate::storage::ShardWriter;
    use crate::telemetry::Stats;

    const ARTICLE: &str = "# Kıyılar\n\nTürkiye üç tarafı denizlerle çevrili bir ülkedir. \
        Karadeniz, Ege ve Akdeniz kıyılarında farklı iklimler görülür. \
        Balıkçılık ve turizm bu kıyılarda önemli bir geçim kaynağıdır.";
    const RELEVANT_TITLE: &str = "Türkiye kıyıları";

    fn document(title: &str, text: &str, url: &str) -> Document {
        Document {
            id: blake3::hash(text.as_bytes()).to_hex().to_string(),
            url: url.to_string(),
            domain: "example.com".to_string(),
            title: title.to_string(),
            language: "tr".to_string(),
            language_score: 0.99,
            word_count: text.split_whitespace().count(),
            char_count: text.chars().count(),
            simhash: "0x0123456789abcdef".to_string(),
            scraped_at: "2026-11-25T22:30:00Z".to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn markdown_markup_is_removed() {
        let markdown = "# Başlık\n\nBir paragraf.\n\n- madde bir\n- madde iki\n\n```rust\nlet x = 1;\n```\n\nA | B\n---\n> alıntı satırı\n\n**kalın** ve `kod`";
        let text = markdown_to_text(markdown);
        assert!(text.starts_with("Başlık\n\nBir paragraf."), "text: {text}");
        assert!(text.contains("madde bir"));
        assert!(!text.contains("- madde"));
        assert!(!text.contains("```"));
        assert!(text.contains("let x = 1;"), "code content kept: {text}");
        assert!(!text.contains("A | B"));
        assert!(!text.contains('#'));
        assert!(text.contains("alıntı satırı"));
        assert!(text.contains("kalın ve kod"));
        assert!(!text.contains('*'));
    }

    #[test]
    fn blank_runs_are_collapsed() {
        assert_eq!(markdown_to_text("a\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn inline_numeric_citations_are_removed() {
        let text = markdown_to_text(
            "Türkiye 1923 yılında cumhuriyet oldu. [ 6] Nüfus sayımı [6, 7] sonuçları açıklandı.",
        );
        assert_eq!(
            text,
            "Türkiye 1923 yılında cumhuriyet oldu. Nüfus sayımı sonuçları açıklandı."
        );
    }

    #[test]
    fn prose_starting_with_an_ordinal_is_not_mistaken_for_a_list() {
        let text = markdown_to_text("18. yüzyılın sonlarında Osmanlı modernleşti.");
        assert_eq!(text, "18. yüzyılın sonlarında Osmanlı modernleşti.");
    }

    #[test]
    fn standalone_citation_markers_are_dropped() {
        let text = markdown_to_text("Bir cümle.\n\n(AEK)\n\n(HA)\n\n[12]\n\n(bkz. kaynakça)");
        assert!(text.contains("Bir cümle."));
        assert!(!text.contains("(AEK)"), "text: {text}");
        assert!(!text.contains("(HA)"), "text: {text}");
        assert!(!text.contains("[12]"), "text: {text}");
        assert!(text.contains("(bkz. kaynakça)"), "text: {text}");
    }

    #[test]
    fn titles_lose_site_suffixes_and_furniture() {
        assert_eq!(clean_title("Türkiye - Vikipedi"), "Türkiye");
        assert_eq!(clean_title("Deprem raporu | BBC News"), "Deprem raporu");
        assert_eq!(clean_title("Devamını Oku"), "");
        assert_eq!(clean_title("Ana Sayfa | Fırat Üniversitesi"), "");
        assert_eq!(
            clean_title("Karar - 2026 değerlendirmesi"),
            "Karar - 2026 değerlendirmesi"
        );
    }

    #[test]
    fn source_urls_and_copyright_lines_are_noise() {
        let markdown = "Bakanlık, Kasım 2026 tarihli bilgi notunda geri dönüş merkezinin kapasitesini ve ilk hedef grubu ayrıntılı biçimde açıklıyor ve yaklaşık yüz kişilik bir kapasite öngörüyor: https://example.org/a.pdf\n\nMedimagazin ® sitesindeki tüm metin ve görsellerin hakları şirkete aittir. © Bu sitedeki içerik izinsiz kullanılamaz.\n\nGerçek cümle burada duruyor ve korunması gerekiyor.";
        let text = markdown_to_text(markdown);
        assert!(!text.contains("https://"), "source url kept: {text}");
        assert!(!text.contains('©'), "copyright kept: {text}");
        assert!(text.contains("Gerçek cümle"));
    }

    #[test]
    fn furniture_headings_are_dropped_from_the_body() {
        let markdown = "## Devamını Oku\n\nGövde metni burada yer alıyor ve korunması gerekiyor.\n\n### Son Dakika\n\nİkinci paragraf da gerçek bir konudan söz ediyor.";
        let text = markdown_to_text(markdown);
        assert!(
            !text.contains("Devamını Oku"),
            "furniture heading kept: {text}"
        );
        assert!(
            !text.contains("Son Dakika"),
            "furniture heading kept: {text}"
        );
        assert!(text.contains("Gövde metni"));
        assert!(text.contains("İkinci paragraf"));
    }

    #[test]
    fn relevant_titles_are_detected() {
        assert!(title_is_relevant(
            "Türkiye - Vikipedi",
            "Türkiye, resmî adıyla Türkiye Cumhuriyeti, kıtalararası bir ülkedir."
        ));
        assert!(title_is_relevant(
            "Türkiye ve denizler",
            "Türkiye üç tarafı denizlerle çevrili bir ülkedir."
        ));
        assert!(!title_is_relevant(
            "Ana Sayfa",
            "Ekonomi haberleri ve piyasa yorumları."
        ));
        assert!(!title_is_relevant("", "Herhangi bir metin."));
        assert!(!title_is_relevant("ve ile bir", "Herhangi bir metin."));
        assert!(!title_is_relevant(
            "Gündem Spor Ekonomi Magazin",
            "Türkiye üç tarafı denizlerle çevrili bir ülkedir."
        ));
    }

    fn jsonl_config() -> OutputConfig {
        OutputConfig {
            format: crate::config::OutputFormat::Jsonl,
            ..OutputConfig::default()
        }
    }

    #[test]
    fn export_writes_plain_text_with_separators_and_gated_titles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &jsonl_config(), stats).expect("writer");
        writer
            .write(&document(
                RELEVANT_TITLE,
                ARTICLE,
                "https://example.com/denizler",
            ))
            .expect("write");
        writer
            .write(&document(
                "Ana Sayfa",
                "# Ekonomi\n\nPiyasa yorumları ve günlük bülten burada yer alır.",
                "https://example.com/ekonomi",
            ))
            .expect("write");
        writer.finalize().expect("finalize");

        let output = dir.path().join("corpus.txt");
        let report = export_txt(
            dir.path(),
            "manifest.json",
            &output,
            &ExportOptions::default(),
        )
        .expect("export");

        let text = std::fs::read_to_string(&output).expect("read corpus");
        assert_eq!(report.documents, 2);
        assert_eq!(report.titles_included, 1);
        assert_eq!(report.titles_dropped, 1);
        assert_eq!(text.matches(DEFAULT_SEPARATOR).count(), 2);
        assert!(
            text.contains("Türkiye kıyıları\n\nKıyılar\n\nTürkiye üç tarafı"),
            "relevant title must precede the body: {text}"
        );
        assert!(
            !text.contains("Ana Sayfa"),
            "irrelevant title leaked: {text}"
        );
        assert!(!text.contains('#'));
        assert!(!text.contains(" | "));
        assert!(report.bytes > 0);
        assert!(report.words > 0);
    }

    #[test]
    fn custom_separator_is_honoured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stats = Arc::new(Stats::new());
        let mut writer = ShardWriter::new(dir.path(), &jsonl_config(), stats).expect("writer");
        writer
            .write(&document(RELEVANT_TITLE, ARTICLE, "https://example.com/a"))
            .expect("write");
        writer.finalize().expect("finalize");
        let output = dir.path().join("corpus.txt");
        let options = ExportOptions {
            separator: "===DOC===".to_string(),
            include_relevant_title: false,
            ..ExportOptions::default()
        };
        export_txt(dir.path(), "manifest.json", &output, &options).expect("export");
        let text = std::fs::read_to_string(&output).expect("read");
        assert!(text.contains("===DOC==="));
        assert!(
            !text.contains(RELEVANT_TITLE),
            "title must be disabled: {text}"
        );
    }

    #[test]
    fn txt_dataset_is_merged_and_separator_can_be_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stats = Arc::new(Stats::new());
        let mut writer =
            ShardWriter::new(dir.path(), &OutputConfig::default(), stats).expect("writer");
        for index in 0..3 {
            let text = format!(
                "# Konu {index}\n\nKonu {index} hakkında yeterince uzun bir gövde metni burada yer alıyor."
            );
            writer
                .write(&document(
                    &format!("Konu {index}"),
                    &text,
                    &format!("https://example.com/{index}"),
                ))
                .expect("write");
        }
        writer.finalize().expect("finalize");

        let output = dir.path().join("merged.txt");
        let options = ExportOptions {
            separator: "===DOC===".to_string(),
            include_relevant_title: true,
            ..ExportOptions::default()
        };
        let report = export_txt(dir.path(), "manifest.json", &output, &options).expect("export");
        let text = std::fs::read_to_string(&output).expect("read");
        assert_eq!(report.documents, 3);
        assert_eq!(text.matches("===DOC===").count(), 3);
        assert!(!text.contains("<veribitisi>"));
        assert!(text.contains("Konu 1"));
    }

    const MULTI_TOPIC: &str = "# Türkiye\n\nTürkiye üç tarafı denizlerle çevrili bir ülkedir.\n\n## Ekonomi\n\nTürkiye ekonomisi sanayi ve hizmet sektörlerine dayanır.\n\n## Kültür\n\nTürk mutfağı zeytinyağı ve tahıla dayanır.";

    #[test]
    fn sections_become_separate_topic_blocks() {
        let document = document("Türkiye", MULTI_TOPIC, "https://example.com/tr");
        let options = ChunkOptions {
            max_words: 100,
            min_words: 3,
            ..ChunkOptions::default()
        };
        let blocks = format_document_blocks(&document, &options);
        assert_eq!(blocks.len(), 3, "blocks: {blocks:#?}");
        assert!(
            blocks[0].text.starts_with("Türkiye\n\nTürkiye üç tarafı"),
            "block0: {:?}",
            blocks[0].text
        );
        assert!(blocks[1].text.contains("Ekonomi") && blocks[1].text.contains("sanayi"));
        assert!(
            !blocks[1].text.contains("mutfağı"),
            "topics leaked: {}",
            blocks[1].text
        );
        assert!(
            !blocks[2].text.contains("sanayi"),
            "topics leaked: {}",
            blocks[2].text
        );
        assert!(blocks[0].title_included);
        assert!(blocks[1..].iter().all(|block| !block.title_included));
        assert!(
            !blocks[1].text.starts_with("Türkiye\n\n"),
            "title repeated: {}",
            blocks[1].text
        );
        assert_eq!(
            blocks
                .iter()
                .filter(|b| b.text.contains(DEFAULT_SEPARATOR))
                .count(),
            3
        );
    }

    #[test]
    fn title_is_written_once_per_document() {
        let document = document("Türkiye", MULTI_TOPIC, "https://example.com/tr");
        let options = ChunkOptions {
            max_words: 100,
            min_words: 3,
            ..ChunkOptions::default()
        };
        let blocks = format_document_blocks(&document, &options);
        assert!(blocks.len() >= 3, "blocks: {blocks:#?}");
        let repetitions = blocks
            .iter()
            .filter(|block| block.text.lines().next() == Some("Türkiye"))
            .count();
        assert_eq!(repetitions, 1, "title repeated: {blocks:#?}");
        assert!(blocks[0].title_included);
        assert!(blocks[1..].iter().all(|block| !block.title_included));
        assert_eq!(blocks.iter().filter(|block| block.title_dropped).count(), 0);
    }

    #[test]
    fn corpus_deduper_drops_repeated_paragraphs_and_blocks() {
        let mut deduper = CorpusDeduper::new(10_000, 6);
        let shared =
            "Bu paragraf korpus genelinde yalnızca bir kez yer almalı ve yeterince uzun bir metin içeriyor.";
        let first = deduper
            .clean_markdown(&format!(
                "# Başlık\n\n{shared}\n\nİkinci paragraf farklı bir konu hakkında bilgi veriyor."
            ))
            .to_string();
        assert!(first.contains("korpus genelinde"));

        let second = deduper
            .clean_markdown(&format!("# Başka\n\n{shared}\n\nYeni bir paragraf burada yer alıyor ve başka şeyler anlatıyor."))
            .to_string();
        assert!(
            !second.contains("korpus genelinde"),
            "duplicate paragraph kept: {second}"
        );
        assert!(second.contains("Yeni bir paragraf"));
        assert_eq!(deduper.removed_paragraphs(), 1);

        let block = "Tekrar eden uzun bir paragraf burada duruyor ve ikinci kez yazılıyor. Sonrasında farklı bir cümle geliyor ve konu değişiyor.";
        assert!(deduper.clean_block(block).is_some());
        assert!(deduper.clean_block(block).is_none());
        assert_eq!(deduper.removed_blocks(), 1);
    }

    #[test]
    fn repeated_sentences_are_removed_from_new_paragraphs() {
        let mut deduper = CorpusDeduper::new(10_000, 6);
        let sentence = "Bu cümle korpus boyunca iki kez geçiyor ve tekrar edilmemesi gerekiyor.";
        let first =
            format!("{sentence} İlk paragrafın ikinci cümlesi tamamen farklıdır ve burada kalır.");
        let second =
            format!("{sentence} İkinci paragrafın kendi cümlesi de farklı bir konudan söz eder.");
        let kept_first = deduper.clean_block(&first).expect("first block");
        assert!(kept_first.contains("İlk paragrafın"));
        let kept_second = deduper.clean_block(&second).expect("second block");
        assert!(
            !kept_second.contains("korpus boyunca"),
            "duplicate sentence kept: {kept_second}"
        );
        assert!(kept_second.contains("İkinci paragrafın"));
        assert!(deduper.removed_sentences() >= 1);
    }

    #[test]
    fn reference_sections_are_not_training_text() {
        let markdown = "# Türkiye\n\nTürkiye bir ülkedir ve bu paragraf yeterince uzundur.\n\n             ## Ayrıca bakınız\n\nTürkiye'nin ana hatları\n\n             ## Notlar\n\n↑ Kürt ve Zazalar toplamı\n\n↑ 6,376,000'ü Türkçe anadillidir\n\n             ## Tarih\n\nTarih bölümü gerçek bir konu anlatır ve burada yer alır.";
        let document = document("Türkiye", markdown, "https://example.com/tr");
        let options = ChunkOptions {
            max_words: 100,
            min_words: 3,
            ..ChunkOptions::default()
        };
        let blocks = format_document_blocks(&document, &options);
        let text: String = blocks.iter().map(|block| block.text.as_str()).collect();
        assert!(!text.contains("Ayrıca bakınız"), "see-also kept: {text}");
        assert!(!text.contains('↑'), "footnotes kept: {text}");
        assert!(text.contains("Tarih bölümü"));
    }

    #[test]
    fn dates_and_metadata_labels_are_dropped() {
        let text = markdown_to_text(
            "21 Haziran 2017\n\nHaber Yeri\n\nAnkara\n\nGövde metni burada yer alıyor ve anlamlıdır.\n\n24 Kasım 2026",
        );
        assert!(!text.contains("Haziran"), "date kept: {text}");
        assert!(!text.contains("Haber Yeri"), "label kept: {text}");
        assert!(!text.contains("Ankara"), "label value kept: {text}");
        assert!(!text.contains("24 Kasım"), "date kept: {text}");
        assert!(text.contains("Gövde metni"));
    }

    #[test]
    fn social_label_block_is_dropped_but_following_prose_is_kept() {
        let markdown = "# Başlık\n\nGövde metni burada yer alıyor ve korunmalıdır.\n\nTwitter hesaplarımız:\n\nzkuşağı\n\nhayrikozanoğlu\n\nAKP\n\nSon cümle de korunmalıdır.";
        let cleaned = sanitize_markdown(markdown);
        assert!(cleaned.contains("Gövde metni"));
        assert!(cleaned.contains("Son cümle"));
        for junk in ["Twitter hesaplarımız", "zkuşağı", "hayrikozanoğlu", "AKP"] {
            assert!(!cleaned.contains(junk), "{junk} kept: {cleaned}");
        }
    }

    #[test]
    fn short_tail_section_merges_back_into_the_previous_one() {
        let long = "kelime ".repeat(60);
        let markdown = format!("# Başlık\n\n{long}\n\n## Kısa\n\nbeş kelimelik kısa kuyruk");
        let sections = topic_sections(&markdown, 3000, 25);
        assert_eq!(
            sections.len(),
            1,
            "short tail must merge back: {sections:?}"
        );
        assert!(sections[0].words >= 60);
    }

    #[test]
    fn sanitize_markdown_keeps_structure_and_drops_furniture() {
        let markdown = "# Başlık\n\n↑ Kaynak. Erişim tarihi: 3 Mayıs 2019.\n\n- madde bir\n- madde iki\n\nGövde metni burada yer alıyor. [6] Nüfus sayımı [6, 7] açıklandı.\n\nAna Sayfa";
        let cleaned = sanitize_markdown(markdown);
        assert!(cleaned.contains("# Başlık"), "heading lost: {cleaned}");
        assert!(cleaned.contains("- madde bir"), "list lost: {cleaned}");
        assert!(
            cleaned.contains("Gövde metni burada yer alıyor. Nüfus sayımı açıklandı."),
            "prose/citations wrong: {cleaned}"
        );
        assert!(
            !cleaned.contains("Erişim tarihi"),
            "reference kept: {cleaned}"
        );
        assert!(!cleaned.contains("Ana Sayfa"), "furniture kept: {cleaned}");
        assert!(!cleaned.contains("[6"), "citation kept: {cleaned}");
    }

    #[test]
    fn bibliography_links_and_furniture_lines_are_dropped() {
        let markdown = "# Başlık\n\n↑ Kaynak, sayfa 12.\n\nGerçek gövde metni burada yer alıyor ve bu kayıt korunmalıdır.\n\n2 Ağustos 2025 tarihinde kaynağından arşivlendi. Erişim tarihi: 16 Temmuz 2025.\n\nISBN 978-975-0000-00-0\n\nNeue Doku ist raus. #Daltons #Bandenkrieg https://t.co/j3vHExsLRG\n\n© 2026 Örnek Yayın. Tüm hakları saklıdır.\n\nAna Sayfa\n\nDevamını oku\n\nSon cümle de burada duruyor ve anlamlıdır.";
        let text = markdown_to_text(markdown);
        assert!(text.contains("Gerçek gövde"));
        assert!(text.contains("Son cümle"));
        for needle in [
            "↑",
            "Erişim tarihi",
            "arşivlendi",
            "ISBN",
            "t.co",
            "Tüm hakları",
            "Ana Sayfa",
            "Devamını oku",
        ] {
            assert!(!text.contains(needle), "{needle} kept: {text}");
        }
    }

    #[test]
    fn plain_blocks_lose_noise_lines_on_export() {
        let mut deduper = CorpusDeduper::new(10_000, 6);
        let block = "Gerçek gövde metni burada yer alıyor ve yeterince uzun bir cümle içeriyor.\n\n↑ Kaynak. Erişim tarihi: 3 Mayıs 2019.\n\nhttps://t.co/abc";
        let cleaned = deduper.clean_block(block).expect("body must survive");
        assert!(cleaned.contains("Gerçek gövde"));
        assert!(
            !cleaned.contains("Erişim tarihi"),
            "reference kept: {cleaned}"
        );
        assert!(!cleaned.contains("t.co"), "link kept: {cleaned}");
    }

    #[test]
    fn short_sections_survive_as_their_own_record() {
        let first = "Türkiye üç tarafı denizlerle çevrili bir ülkedir ve coğrafyası dağlar ovalar \
            akarsular bakımından oldukça çeşitlilik gösterir. Yüksek platolar iç kesimlerde geniş \
            yer kaplar ve nehirler farklı denizlere dökülür.";
        let second = "Türkiye ekonomisi sanayi hizmet ve tarım sektörlerine dayanır; ihracat içinde \
            otomotiv tekstil ve turizm önemli bir yer tutar. Son yıllarda yazılım ve savunma sanayisi \
            de hızla büyümektedir.";
        let third = "Türk mutfağı zeytinyağı tahıl ve sebze ağırlıklıdır; kebap börek ve tatlılar \
            ülkenin farklı bölgelerinde farklı biçimlerde hazırlanır. Kahvaltı sofraları peynir zeytin \
            ve sıcak ekmekle zenginleşir.";
        let markdown =
            format!("# Türkiye\n\n{first}\n\n## Ekonomi\n\n{second}\n\n## Kültür\n\n{third}");
        let document = document("Türkiye", &markdown, "https://example.com/tr");
        let options = ChunkOptions {
            max_words: 3_000,
            min_words: 25,
            ..ChunkOptions::default()
        };
        let blocks = format_document_blocks(&document, &options);
        assert_eq!(blocks.len(), 3, "blocks: {blocks:#?}");
        assert!(blocks[1].text.contains("Ekonomi"));
        assert!(blocks[2].text.contains("Kültür"));
    }

    #[test]
    fn tiny_sections_are_merged_but_kept_apart_from_distant_topics() {
        let document = document("Türkiye", MULTI_TOPIC, "https://example.com/tr");
        let options = ChunkOptions {
            max_words: 100,
            min_words: 80,
            ..ChunkOptions::default()
        };
        let blocks = format_document_blocks(&document, &options);
        assert_eq!(blocks.len(), 1, "small sections must be packed together");
        assert!(blocks[0].text.contains("Ekonomi"));
        assert!(blocks[0].text.contains("Kültür"));
    }

    #[test]
    fn long_sections_are_split_under_the_cap() {
        let sentence = "Bu cümle konu hakkında kısa bir bilgi verir.";
        let mut body = String::from("## Uzun bölüm\n\n");
        for _ in 0..80 {
            body.push_str(sentence);
            body.push(' ');
        }
        let document = document("Uzun", &body, "https://example.com/uzun");
        let options = ChunkOptions {
            max_words: 60,
            min_words: 5,
            ..ChunkOptions::default()
        };
        let blocks = format_document_blocks(&document, &options);
        assert!(
            blocks.len() >= 3,
            "expected several blocks, got {}",
            blocks.len()
        );
        for block in &blocks {
            assert!(block.words <= 63, "block too large: {} words", block.words);
        }
        let total: u64 = blocks.iter().map(|block| block.words).sum();
        let expected = body.split_whitespace().count() as u64;
        assert!(total >= expected, "no content may be dropped");
    }

    #[test]
    fn missing_manifest_and_empty_separator_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("corpus.txt");
        assert!(export_txt(
            dir.path(),
            "manifest.json",
            &output,
            &ExportOptions::default()
        )
        .is_err());

        let options = ExportOptions {
            separator: "   ".to_string(),
            include_relevant_title: true,
            ..ExportOptions::default()
        };
        assert!(export_txt(dir.path(), "manifest.json", &output, &options).is_err());
    }
}
