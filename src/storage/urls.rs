//! Cross-run log of content pages that were already saved.
//!
//! The frontier database remembers visited URLs for one crawl database, but a
//! fresh database (or a different machine) would fetch every page again. This
//! module keeps a portable text file with one full content-page URL per line,
//! so a later crawl can load it and skip those pages before any request is made.
//!
//! Only *content* pages are recorded: the home page, section/category pages,
//! tag/archive/search listings and pagination URLs are excluded by
//! [`is_content_page`]. The memory index keeps one 64-bit hash per URL, so a
//! million saved pages cost a few tens of megabytes instead of hundreds.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use url::Url;
use xxhash_rust::xxh3::xxh3_64;

use crate::error::{Result, ScraperError};

/// Default file name inside the output directory.
pub const DEFAULT_SAVED_URLS_FILE: &str = "crawled_urls.txt";
/// How many URL hashes are kept in memory; beyond this the index stops growing
/// but the file keeps receiving new lines (constant memory by design).
const INDEX_CAPACITY: usize = 5_000_000;
/// How many new URLs are buffered before they are flushed to disk.
const FLUSH_EVERY: usize = 256;

/// First path segments that mark a listing, never a content page.
const STRUCTURAL_SEGMENTS: &[&str] = &[
    "kategori",
    "kategoriler",
    "category",
    "categories",
    "etiket",
    "etiketler",
    "tag",
    "tags",
    "arsiv",
    "archive",
    "arama",
    "search",
    "yazarlar",
    "author",
    "authors",
    "galeri",
    "foto-galeri",
    "fotogaleri",
    "page",
    "sayfa",
];

/// Home pages and listing entry points that must stay out of the skip list.
const HOMEPAGE_NAMES: &[&str] = &["index.html", "index.htm", "index.php", "default.aspx"];

/// True when `url` is a deep content page rather than a home/section/listing page.
///
/// Rules, in order: no home page (`/`), no trailing slash (directory-style
/// listing), no pagination/filter query, no structural first segment
/// (`/kategori/...`, `/etiket/...`, `/arama/...`) and a single plain segment
/// (`/ekonomi`) is a section, while a slug with a separator, a digit or a page
/// extension (`/enflasyon-raporu`, `/haber/12345`, `/x.html`) is content.
pub fn is_content_page(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let path = url.path();
    let lower_path = path.to_ascii_lowercase();
    if path.is_empty() || path == "/" || lower_path.ends_with('/') {
        return false;
    }
    if let Some(query) = url.query() {
        for (key, _) in url.query_pairs() {
            let key = key.to_ascii_lowercase();
            if key == "page" || key == "sayfa" || key == "p" || key == "offset" {
                return false;
            }
        }
        let lower_query = query.to_ascii_lowercase();
        if lower_query.contains("page=") || lower_query.contains("sayfa=") {
            return false;
        }
    }
    let segments: Vec<&str> = lower_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let Some(first) = segments.first() else {
        return false;
    };
    if STRUCTURAL_SEGMENTS.contains(first) {
        return false;
    }
    let last = segments.last().copied().unwrap_or("");
    if HOMEPAGE_NAMES.contains(&last) {
        return false;
    }
    if segments.len() == 1 {
        let slug = segments[0];
        let article_like = slug.contains('-')
            || slug.chars().any(|c| c.is_ascii_digit())
            || slug.ends_with(".html")
            || slug.ends_with(".htm")
            || slug.ends_with(".shtml")
            || slug.ends_with(".php");
        if !article_like {
            return false;
        }
    }
    true
}

/// Append-only, bounded-memory index of saved content URLs.
#[derive(Debug)]
pub struct SavedUrls {
    path: PathBuf,
    seen: HashSet<u64>,
    pending: Vec<String>,
    saturated: bool,
}

impl SavedUrls {
    /// Open (or create) `dir/file_name` and load the URLs already listed.
    pub fn open(dir: &Path, file_name: &str) -> Result<Self> {
        let path = dir.join(file_name);
        let mut seen = HashSet::new();
        if path.exists() {
            let file = std::fs::File::open(&path).map_err(|e| ScraperError::io_at(&path, e))?;
            for line in BufReader::new(file).lines() {
                let line = line.map_err(|e| ScraperError::io_at(&path, e))?;
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if seen.len() >= INDEX_CAPACITY {
                    break;
                }
                seen.insert(hash_url(line));
            }
        }
        let saturated = seen.len() >= INDEX_CAPACITY;
        Ok(Self {
            path,
            seen,
            pending: Vec::new(),
            saturated,
        })
    }

    /// True when this exact page is already in the saved list.
    pub fn contains(&self, url: &str) -> bool {
        self.seen.contains(&hash_url(url))
    }

    /// True when a URL is a content page and was not listed before.
    pub fn should_record(&self, url: &str) -> bool {
        matches!(Url::parse(url), Ok(parsed) if is_content_page(&parsed)) && !self.contains(url)
    }

    /// Remember a saved page. Home pages, section/listing pages and non-http
    /// URLs are ignored; repeated URLs are written once.
    pub fn record(&mut self, url: &str) -> Result<bool> {
        if !self.should_record(url) {
            return Ok(false);
        }
        let key = hash_url(url);
        if !self.saturated {
            if self.seen.len() < INDEX_CAPACITY {
                self.seen.insert(key);
            } else {
                self.saturated = true;
            }
        }
        self.pending.push(url.to_string());
        if self.pending.len() >= FLUSH_EVERY {
            self.flush()?;
        }
        Ok(true)
    }

    /// Write buffered URLs to disk.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| ScraperError::io_at(&self.path, e))?;
        let mut writer = std::io::BufWriter::new(file);
        for url in self.pending.drain(..) {
            writer
                .write_all(url.as_bytes())
                .and_then(|()| writer.write_all(b"\n"))
                .map_err(|e| ScraperError::io_at(&self.path, e))?;
        }
        writer
            .flush()
            .map_err(|e| ScraperError::io_at(&self.path, e))?;
        Ok(())
    }

    /// Path of the URL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of URLs currently indexed.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// True when the index holds no URLs.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

fn hash_url(url: &str) -> u64 {
    xxh3_64(url.trim().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("valid url")
    }

    #[test]
    fn home_and_section_pages_are_not_content() {
        for raw in [
            "https://example.com/",
            "https://example.com",
            "https://example.com/ekonomi",
            "https://example.com/ekonomi/",
            "https://example.com/kategori/spor",
            "https://example.com/etiket/secim",
            "https://example.com/arama?q=test",
            "https://example.com/haberler?sayfa=2",
            "https://example.com/index.html",
            "file:///tmp/page.html",
        ] {
            assert!(!is_content_page(&url(raw)), "{raw} must not be content");
        }
    }

    #[test]
    fn article_urls_are_content() {
        for raw in [
            "https://example.com/haber/12345",
            "https://example.com/2026/11/26/enflasyon-raporu",
            "https://example.com/enflasyon-raporu",
            "https://example.com/haber/baslik.html",
            "https://example.com/ekonomi/enflasyon-raporu",
        ] {
            assert!(is_content_page(&url(raw)), "{raw} must be content");
        }
    }

    #[test]
    fn saved_urls_are_recorded_once_and_reloaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut saved = SavedUrls::open(dir.path(), "crawled_urls.txt").expect("open");
        assert!(saved.record("https://example.com/haber/1").expect("record"));
        assert!(saved
            .record("https://example.com/2026/11/26/yazi")
            .expect("record"));
        assert!(!saved.record("https://example.com/").expect("home ignored"));
        assert!(!saved
            .record("https://example.com/kategori/spor")
            .expect("listing ignored"));
        assert!(!saved
            .record("https://example.com/haber/1")
            .expect("duplicate ignored"));
        saved.flush().expect("flush");
        drop(saved);

        let reloaded = SavedUrls::open(dir.path(), "crawled_urls.txt").expect("reopen");
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains("https://example.com/haber/1"));
        assert!(reloaded.contains("https://example.com/2026/11/26/yazi"));
        assert!(!reloaded.contains("https://example.com/"));
    }
}
