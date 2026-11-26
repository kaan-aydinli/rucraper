//! Heuristic quality rules derived from Gopher, C4 and FineWeb.
//!
//! All rules are cheap, deterministic and operate on a single pass over the
//! text (or a handful of `str::lines` passes), which keeps the filter stage
//! comfortably faster than extraction and dedup.

use crate::config::FilterConfig;
use crate::filter::RejectReason;

const SENTENCE_ENDINGS: &[char] = &['.', '!', '?', ':', '…', '。', '！', '？', '؛', '।'];
const TRAILING_WRAPPERS: &[char] = &[
    '"', '\'', ')', ']', '}', '”', '’', '»', '*', '_', '`', '>', '。', '…',
];
const SYMBOLS: &[char] = &[
    '#', '@', '$', '%', '*', '|', '^', '~', '<', '>', '{', '}', '[', ']', '\\', '/', '=', '+',
];

/// Word level statistics used by several rules.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WordStats {
    /// Number of whitespace separated tokens.
    pub words: usize,
    /// Mean token length in characters.
    pub avg_len: f64,
    /// Fraction of tokens containing at least one alphabetic character.
    pub alpha_ratio: f64,
    /// Symbol characters divided by the token count.
    pub symbol_ratio: f64,
}

/// Compute the word statistics of `text`.
pub fn word_stats(text: &str) -> WordStats {
    let mut words = 0usize;
    let mut total_chars = 0usize;
    let mut alpha_words = 0usize;
    for token in text.split_whitespace() {
        words += 1;
        let length = token.chars().count();
        total_chars += length;
        if token.chars().any(|c| c.is_alphabetic()) {
            alpha_words += 1;
        }
    }
    let symbols = text.chars().filter(|c| SYMBOLS.contains(c)).count();
    WordStats {
        words,
        avg_len: if words == 0 {
            0.0
        } else {
            total_chars as f64 / words as f64
        },
        alpha_ratio: if words == 0 {
            0.0
        } else {
            alpha_words as f64 / words as f64
        },
        symbol_ratio: if words == 0 {
            0.0
        } else {
            symbols as f64 / words as f64
        },
    }
}

/// Fraction of non-empty *prose* lines ending with sentence punctuation.
///
/// Markdown structure (headings, code fences and their contents, tables and
/// horizontal rules) is skipped: those lines cannot end with sentence
/// punctuation by design, and counting them would reject every article that
/// contains a code sample. Single-token lines (menu entries, province lists,
/// stray labels) are not prose either and are ignored, so removing noisy
/// bibliography lines cannot push a real article below the threshold.
/// Blockquote markers are stripped before measuring.
pub fn punctuation_line_ratio(text: &str) -> f64 {
    let mut lines = 0usize;
    let mut terminated = 0usize;
    let mut in_fence = false;
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        while let Some(rest) = line.strip_prefix('>') {
            line = rest.trim_start();
        }
        if line.is_empty() || line.starts_with('#') || line == "---" || line.contains(" | ") {
            continue;
        }
        if line.split_whitespace().count() < 2 {
            continue;
        }
        lines += 1;
        let candidate: String = line
            .chars()
            .rev()
            .skip_while(|c| TRAILING_WRAPPERS.contains(c) && !SENTENCE_ENDINGS.contains(c))
            .collect::<Vec<char>>()
            .into_iter()
            .rev()
            .collect();
        if candidate
            .chars()
            .last()
            .map(|c| SENTENCE_ENDINGS.contains(&c))
            .unwrap_or(false)
        {
            terminated += 1;
        }
    }
    if lines == 0 {
        0.0
    } else {
        terminated as f64 / lines as f64
    }
}

/// Length and word-shape rules.
pub fn check_shape(text: &str, config: &FilterConfig) -> Result<(), RejectReason> {
    let stats = word_stats(text);
    if stats.words < config.min_words {
        return Err(RejectReason::TooFewWords);
    }
    if stats.words > config.max_words {
        return Err(RejectReason::TooManyWords);
    }
    if stats.avg_len < config.min_avg_word_len || stats.avg_len > config.max_avg_word_len {
        return Err(RejectReason::AvgWordLength);
    }
    Ok(())
}

/// Punctuation, alphabetic content and symbol density rules.
pub fn check_syntax(text: &str, config: &FilterConfig) -> Result<(), RejectReason> {
    if punctuation_line_ratio(text) < config.min_punctuation_line_ratio {
        return Err(RejectReason::LowPunctuation);
    }
    let stats = word_stats(text);
    if stats.alpha_ratio < config.min_alpha_word_ratio {
        return Err(RejectReason::LowAlphaWords);
    }
    if stats.symbol_ratio > config.max_symbol_word_ratio {
        return Err(RejectReason::HighSymbolRatio);
    }
    Ok(())
}

/// Reject template/error pages by phrase.
pub fn check_blocked_phrases(text: &str, config: &FilterConfig) -> Result<(), RejectReason> {
    if config.blocked_phrases.is_empty() {
        return Ok(());
    }
    let lowered = text.to_ascii_lowercase();
    for phrase in &config.blocked_phrases {
        let needle = phrase.trim().to_ascii_lowercase();
        if !needle.is_empty() && lowered.contains(&needle) {
            return Err(RejectReason::BlockedPhrase);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generous() -> FilterConfig {
        FilterConfig {
            min_words: 30,
            ..FilterConfig::default()
        }
    }

    const PROSE: &str = "The compiler verifies ownership and borrowing at compile time. \
        This makes concurrent programs much easier to reason about. \
        Memory is released deterministically when values go out of scope. \
        Error messages point at the exact expression that violated a rule. \
        Teams adopt the language for services that must stay responsive. \
        Tooling such as cargo keeps builds reproducible and fast.";

    #[test]
    fn prose_passes_shape_and_syntax() {
        let config = generous();
        assert!(check_shape(PROSE, &config).is_ok());
        assert!(check_syntax(PROSE, &config).is_ok());
        assert!(check_blocked_phrases(PROSE, &config).is_ok());
    }

    #[test]
    fn short_text_is_rejected() {
        let config = generous();
        assert_eq!(
            check_shape("Too short to be useful.", &config),
            Err(RejectReason::TooFewWords)
        );
    }

    #[test]
    fn menu_soup_fails_punctuation_rule() {
        let menu =
            "Home\nProducts\nPricing\nAbout us\nContact\nBlog\nCareers\nPress\nLogin\nSign up";
        let config = generous();
        assert_eq!(
            check_syntax(menu, &config),
            Err(RejectReason::LowPunctuation)
        );
    }

    #[test]
    fn symbol_soup_fails_symbol_rule() {
        let soup = "$$$ @@@ ### ||| ^^^ ~~~ <<< >>> {{{ }}} [[[ ]]] \\\\\\ /// === +++ $$$ @@@ ###";
        let config = FilterConfig {
            min_words: 5,
            min_avg_word_len: 1.0,
            max_avg_word_len: 64.0,
            min_punctuation_line_ratio: 0.0,
            min_alpha_word_ratio: 0.0,
            ..FilterConfig::default()
        };
        assert_eq!(
            check_syntax(soup, &config),
            Err(RejectReason::HighSymbolRatio)
        );
    }

    #[test]
    fn blocked_phrases_are_case_insensitive() {
        let config = generous();
        let text = format!("{PROSE} Lorem Ipsum dolor sit amet.");
        assert_eq!(
            check_blocked_phrases(&text, &config),
            Err(RejectReason::BlockedPhrase)
        );
    }

    #[test]
    fn punctuation_ratio_ignores_trailing_quotes() {
        let text = "\"A quoted sentence.\"\nAnother sentence!\nPlain line";
        let ratio = punctuation_line_ratio(text);
        assert!((ratio - 2.0 / 3.0).abs() < 1e-9, "ratio: {ratio}");
    }

    #[test]
    fn single_token_lines_are_not_prose() {
        let text = "Ankara\nKırklareli\nEdirne\nMakale gövdesi burada düzgün bir cümleyle biter.\nİkinci cümle de tamamlanır.";
        assert_eq!(punctuation_line_ratio(text), 1.0);
    }

    #[test]
    fn punctuation_ratio_skips_markdown_structure_and_code() {
        let text = "# Heading without punctuation\n\nA real prose line ends here.\n\n```rust\nlet x = 1;\n```\n\n| a | b |\n\n> A quoted prose line.\n\n---";
        assert_eq!(punctuation_line_ratio(text), 1.0);
    }
}
