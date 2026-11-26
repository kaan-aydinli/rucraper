//! FineWeb / RefinedWeb / Gopher style quality filtering.
//!
//! Filters run in a fixed order so the reported rejection reason is stable and
//! deterministic: length and word shape first (cheap), then punctuation and
//! symbol heuristics, then repetition, then language detection, and finally PII
//! masking on the text that survived. Every rejection produces exactly one
//! [`RejectReason`] that is recorded by the telemetry counters.

pub mod heuristics;
pub mod language;
pub mod pii;
pub mod repetition;

use std::fmt;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::config::FilterConfig;

/// Why a document was rejected. The string form is stable and appears in the
/// manifest reject breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Extraction produced no text at all.
    EmptyContent,
    /// The extracted body is shorter than `extract.min_block_chars`.
    TooShortContent,
    /// Below `filter.min_words`.
    TooFewWords,
    /// Above `filter.max_words`.
    TooManyWords,
    /// Average word length outside the configured band.
    AvgWordLength,
    /// Too few lines end with sentence punctuation.
    LowPunctuation,
    /// Too few words contain letters.
    LowAlphaWords,
    /// Too many symbol characters relative to word count.
    HighSymbolRatio,
    /// A configured blocked phrase was found.
    BlockedPhrase,
    /// Duplicated lines dominate the document.
    DuplicateLines,
    /// One n-gram dominates the document.
    RepetitiveNgrams,
    /// Language could not be determined.
    LanguageUndetermined,
    /// Detected language confidence is below the threshold.
    LowLanguageConfidence,
    /// Detected language is not in `target_languages`.
    UnsupportedLanguage,
    /// Identical document already seen (XXH3-128).
    ExactDuplicate,
    /// Near duplicate (SimHash Hamming distance within the limit).
    NearDuplicate,
    /// robots.txt disallowed the URL.
    RobotsBlocked,
    /// Response was not HTML.
    NonHtmlResponse,
    /// Response body exceeded the configured limit.
    ResponseTooLarge,
    /// Fetch failed (network or HTTP error).
    FetchFailed,
    /// A document produced no usable title and body combination (reserved).
    UnusableDocument,
}

impl RejectReason {
    /// All reasons in a stable order; the index is used by the atomic counters.
    pub const ALL: &'static [RejectReason] = &[
        RejectReason::EmptyContent,
        RejectReason::TooShortContent,
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
        RejectReason::ExactDuplicate,
        RejectReason::NearDuplicate,
        RejectReason::RobotsBlocked,
        RejectReason::NonHtmlResponse,
        RejectReason::ResponseTooLarge,
        RejectReason::FetchFailed,
        RejectReason::UnusableDocument,
    ];

    /// Stable snake_case identifier.
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::EmptyContent => "empty_content",
            RejectReason::TooShortContent => "too_short_content",
            RejectReason::TooFewWords => "too_few_words",
            RejectReason::TooManyWords => "too_many_words",
            RejectReason::AvgWordLength => "avg_word_length",
            RejectReason::LowPunctuation => "low_punctuation",
            RejectReason::LowAlphaWords => "low_alpha_words",
            RejectReason::HighSymbolRatio => "high_symbol_ratio",
            RejectReason::BlockedPhrase => "blocked_phrase",
            RejectReason::DuplicateLines => "duplicate_lines",
            RejectReason::RepetitiveNgrams => "repetitive_ngrams",
            RejectReason::LanguageUndetermined => "language_undetermined",
            RejectReason::LowLanguageConfidence => "low_language_confidence",
            RejectReason::UnsupportedLanguage => "unsupported_language",
            RejectReason::ExactDuplicate => "exact_duplicate",
            RejectReason::NearDuplicate => "near_duplicate",
            RejectReason::RobotsBlocked => "robots_blocked",
            RejectReason::NonHtmlResponse => "non_html_response",
            RejectReason::ResponseTooLarge => "response_too_large",
            RejectReason::FetchFailed => "fetch_failed",
            RejectReason::UnusableDocument => "unusable_document",
        }
    }

    /// Index into the telemetry counter array.
    pub fn index(&self) -> usize {
        Self::ALL
            .iter()
            .position(|reason| reason == self)
            .unwrap_or(0)
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Document that survived extraction and every quality filter.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanDocument {
    /// Markdown text after normalization and PII masking.
    pub text: String,
    /// ISO language code (ISO-639-1 when known, otherwise ISO-639-3 or `und`).
    pub language: String,
    /// Language detection confidence in `[0, 1]`.
    pub language_score: f64,
    /// Whitespace separated token count.
    pub word_count: usize,
    /// Character count.
    pub char_count: usize,
}

/// Run every quality filter over extracted text.
///
/// Returns the cleaned document or the first [`RejectReason`] encountered.
pub fn evaluate(
    text: &str,
    config: &FilterConfig,
) -> std::result::Result<CleanDocument, RejectReason> {
    let normalized = normalize_text(text);
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return Err(RejectReason::EmptyContent);
    }

    heuristics::check_shape(trimmed, config)?;
    heuristics::check_syntax(trimmed, config)?;
    heuristics::check_blocked_phrases(trimmed, config)?;
    repetition::check(trimmed, config)?;

    let (language, language_score) = language::detect_and_check(trimmed, config)?;

    let (final_text, masked) = if config.enable_pii_masking {
        pii::mask_pii(trimmed)
    } else {
        (trimmed.to_string(), 0)
    };
    if masked > 0 {
        tracing::debug!(masked, "masked PII entities");
    }

    let word_count = final_text.split_whitespace().count();
    let char_count = final_text.chars().count();
    Ok(CleanDocument {
        text: final_text,
        language,
        language_score,
        word_count,
        char_count,
    })
}

/// NFKC-normalize and drop invisible/control characters, keeping line structure.
pub fn normalize_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.nfkc() {
        match ch {
            '\r' => {}
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}' | '\u{00ad}' => {}
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose() -> String {
        "Rust is a systems programming language that runs blazingly fast and prevents segfaults. \
         It achieves memory safety without a garbage collector by tracking ownership at compile time. \
         The borrow checker validates every reference, which removes data races and use after free bugs. \
         Teams that adopt it report fewer production incidents and predictable latency under load. \
         Compiler error messages are unusually helpful and guide newcomers toward idiomatic designs."
            .to_string()
    }

    #[test]
    fn real_prose_passes_every_filter() {
        let config = FilterConfig {
            min_words: 50,
            ..FilterConfig::default()
        };
        let doc = evaluate(&prose(), &config).expect("prose accepted");
        assert_eq!(doc.language, "en");
        assert!(doc.language_score > 0.5);
        assert!(doc.word_count >= 50);
        assert_eq!(doc.char_count, doc.text.chars().count());
    }

    #[test]
    fn empty_text_is_rejected() {
        assert_eq!(
            evaluate("   \n\t ", &FilterConfig::default()),
            Err(RejectReason::EmptyContent)
        );
    }

    #[test]
    fn reject_reasons_have_stable_indices_and_strings() {
        for (index, reason) in RejectReason::ALL.iter().enumerate() {
            assert_eq!(reason.index(), index);
            assert!(!reason.as_str().is_empty());
        }
        assert_eq!(RejectReason::NearDuplicate.as_str(), "near_duplicate");
    }

    #[test]
    fn unicode_normalization_removes_zero_width_characters() {
        let normalized = normalize_text("a\u{200b}b\u{feff}c\r\nd");
        assert_eq!(normalized, "abc\nd");
    }
}
