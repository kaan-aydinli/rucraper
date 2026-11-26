//! Repetition detection (FineWeb / Gopher style).
//!
//! Two independent signals are computed in one pass each:
//! * duplicated non-empty lines (navigation templates, repeated boilerplate),
//! * the share of the text taken by the single most frequent n-gram (SEO spam,
//!   keyword stuffing, "page 1 of 400" archives).
//!
//! Both are O(n) in the document size and allocation-bounded by the number of
//! distinct lines / n-grams, which is small for real prose.

use std::collections::HashMap;

use crate::config::FilterConfig;
use crate::filter::RejectReason;

/// Repetition measurements for a document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RepetitionStats {
    /// Repeated lines / total non-empty lines.
    pub duplicate_line_ratio: f64,
    /// Characters inside repeated lines / total characters.
    pub duplicate_char_ratio: f64,
    /// Occurrences of the most frequent n-gram / total n-grams.
    pub top_ngram_ratio: f64,
    /// Repeated n-grams / total n-grams (catches any repeated template phrase).
    pub duplicate_ngram_ratio: f64,
}

/// Compute all repetition statistics.
pub fn analyze(text: &str, ngram_size: usize) -> RepetitionStats {
    let mut line_counts: HashMap<&str, u32> = HashMap::new();
    let mut lines = 0u64;
    let mut chars = 0u64;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        chars += trimmed.chars().count() as u64;
        *line_counts.entry(trimmed).or_insert(0) += 1;
    }
    let mut repeated_lines = 0u64;
    let mut repeated_chars = 0u64;
    for (line, count) in &line_counts {
        if *count > 1 {
            repeated_lines += (*count - 1) as u64;
            repeated_chars += (count - 1) as u64 * line.chars().count() as u64;
        }
    }
    let duplicate_line_ratio = if lines == 0 {
        0.0
    } else {
        repeated_lines as f64 / lines as f64
    };
    let duplicate_char_ratio = if chars == 0 {
        0.0
    } else {
        repeated_chars as f64 / chars as f64
    };

    let (top_ngram_ratio, duplicate_ngram_ratio) = ngram_stats(text, ngram_size.max(1));
    RepetitionStats {
        duplicate_line_ratio,
        duplicate_char_ratio,
        top_ngram_ratio,
        duplicate_ngram_ratio,
    }
}

/// Share of the most frequent n-gram over all n-grams.
pub fn top_ngram_ratio(text: &str, ngram_size: usize) -> f64 {
    ngram_stats(text, ngram_size).0
}

/// Share of n-grams that occur more than once (1 - unique/total).
pub fn duplicate_ngram_ratio(text: &str, ngram_size: usize) -> f64 {
    ngram_stats(text, ngram_size).1
}

fn ngram_stats(text: &str, ngram_size: usize) -> (f64, f64) {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(|token| {
            token
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.len() < ngram_size {
        return (0.0, 0.0);
    }
    let total = tokens.len() - ngram_size + 1;
    let mut counts: HashMap<u64, u32> = HashMap::with_capacity(total);
    for window in tokens.windows(ngram_size) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for token in window {
            std::hash::Hash::hash(token, &mut hasher);
        }
        let key = std::hash::Hasher::finish(&hasher);
        *counts.entry(key).or_insert(0) += 1;
    }
    let max = counts.values().copied().max().unwrap_or(0) as f64;
    let unique = counts.len() as f64;
    if total == 0 {
        return (0.0, 0.0);
    }
    (max / total as f64, 1.0 - unique / total as f64)
}

/// Reject documents dominated by duplicated lines or repeated n-grams.
pub fn check(text: &str, config: &FilterConfig) -> Result<(), RejectReason> {
    let stats = analyze(text, config.ngram_size);
    if stats.duplicate_line_ratio > config.max_duplicate_line_ratio
        || stats.duplicate_char_ratio > 0.5
    {
        return Err(RejectReason::DuplicateLines);
    }
    if stats.top_ngram_ratio > config.max_ngram_repetition_ratio
        || stats.duplicate_ngram_ratio > config.max_ngram_repetition_ratio
    {
        return Err(RejectReason::RepetitiveNgrams);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VARIED: &str = "Rust provides memory safety without a garbage collector. \
        The borrow checker validates references before the program runs. \
        Cargo builds reproducible artifacts and manages dependencies. \
        Async runtimes schedule many tasks on a small thread pool. \
        Error handling is explicit and typesafe through the Result enum. \
        Tooling integrates with editors for instant feedback.";

    #[test]
    fn varied_prose_has_low_repetition() {
        let config = FilterConfig::default();
        let stats = analyze(VARIED, 5);
        assert!(stats.duplicate_line_ratio < 0.1);
        assert!(stats.top_ngram_ratio < 0.1);
        assert!(
            stats.duplicate_ngram_ratio < 0.2,
            "duplicate n-gram ratio: {}",
            stats.duplicate_ngram_ratio
        );
        assert!(check(VARIED, &config).is_ok());
    }

    #[test]
    fn duplicated_lines_are_rejected() {
        let line = "Buy the best product at the lowest price available today.";
        let text = std::iter::repeat(line)
            .take(30)
            .collect::<Vec<_>>()
            .join("\n");
        let stats = analyze(&text, 5);
        assert!(stats.duplicate_line_ratio > 0.9);
        assert_eq!(
            check(&text, &FilterConfig::default()),
            Err(RejectReason::DuplicateLines)
        );
    }

    #[test]
    fn keyword_stuffing_is_rejected() {
        let mut text = String::new();
        for i in 0..80 {
            if i % 30 == 0 {
                text.push_str("A unique sentence appears here with distinct wording. ");
            }
            text.push_str("cheap flights to anywhere cheap flights to anywhere. ");
        }
        let stats = analyze(&text, 5);
        assert!(
            stats.top_ngram_ratio > 0.3 || stats.duplicate_ngram_ratio > 0.3,
            "top: {}, duplicate: {}",
            stats.top_ngram_ratio,
            stats.duplicate_ngram_ratio
        );
        assert_eq!(
            check(&text, &FilterConfig::default()),
            Err(RejectReason::RepetitiveNgrams)
        );
    }
}
