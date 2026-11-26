//! Central, typed error hierarchy for every stage of the pipeline.
//!
//! Every fallible operation in the crate returns [`Result<T>`], which is an
//! alias for `std::result::Result<T, ScraperError>`. Errors are kept small and
//! cheap to construct because they travel through the hot path of the crawl
//! loop (where a per-URL failure is an expected event, not a crash).

use std::path::PathBuf;

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, ScraperError>;

/// Every error this program can produce.
#[derive(Debug, Error)]
pub enum ScraperError {
    /// Configuration file is malformed or semantically invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// I/O failure tied to a specific path (helps resumable-state debugging).
    #[error("i/o error at {path}: {source}")]
    Io {
        /// Path that was being touched when the failure happened.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// I/O failure without a meaningful path (e.g. a stream sink).
    #[error("i/o error: {0}")]
    IoBare(#[from] std::io::Error),

    /// HTTP transport failure surfaced by `reqwest`.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// URL parsing failure.
    #[error("url parse error: {0}")]
    Url(#[from] url::ParseError),

    /// Embedded database (LSM tree) failure.
    #[error("database error: {0}")]
    Db(#[from] sled::Error),

    /// JSON serialization / deserialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML decoding failure.
    #[error("toml decode error: {0}")]
    Toml(#[from] toml::de::Error),

    /// Zstandard compression / decompression failure.
    #[error("zstd error: {0}")]
    Zstd(String),

    /// A compiled regular expression is invalid.
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    /// A single URL could not be fetched; the crawl continues.
    #[error("fetch failed for {url}: {reason}")]
    Fetch {
        /// Offending URL.
        url: String,
        /// Human readable reason (already classified by the fetcher).
        reason: String,
    },

    /// HTML extraction failed in an unrecoverable way.
    #[error("extraction error: {0}")]
    Extract(String),

    /// Shard writing / manifest management failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// A background task panicked or was cancelled.
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Environment problem (missing file, unusable directory, ...).
    #[error("{0}")]
    Other(String),
}

impl ScraperError {
    /// Attach a path to an [`std::io::Error`].
    pub fn io_at(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Convenience constructor for configuration problems.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Convenience constructor for storage problems.
    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage(message.into())
    }
}
