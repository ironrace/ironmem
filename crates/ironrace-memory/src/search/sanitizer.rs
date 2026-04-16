//! Query sanitizer — prevents system prompt contamination from collapsing retrieval.
//!
//! When AI agents prepend system prompts (2000+ chars) to search queries,
//! embedding models fail catastrophically: 89.8% → 1.0% R@10.
//!
//! 4-step mitigation:
//!   1. Passthrough (≤200 chars)    → no degradation
//!   2. Question extraction          → near-full recovery
//!   3. Tail sentence extraction     → moderate recovery
//!   4. Tail truncation (fallback)   → minimum viable (~70-80%)

use regex::Regex;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::LazyLock;

const MAX_QUERY_LENGTH: usize = 500;
const SAFE_QUERY_LENGTH: usize = 200;
const MIN_QUERY_LENGTH: usize = 10;

/// Safely take the tail of a string without panicking on multi-byte UTF-8 boundaries.
fn safe_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let start = s.len() - max_bytes;
    let start = s.ceil_char_boundary(start);
    &s[start..]
}

static QUESTION_MARK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"[?？]\s*["']?\s*$"#).unwrap());

/// English stop words to strip when extracting content words for multi-query search.
static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
        "from", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had", "do",
        "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can", "not",
        "no", "what", "when", "where", "who", "which", "how", "why", "that", "this", "these",
        "those", "there", "here", "i", "me", "my", "you", "your", "he", "she", "it", "we", "they",
        "their", "our", "its", "him", "her", "us", "them", "about", "any", "some", "all", "just",
        "more", "also", "than", "then", "into", "up", "out", "if", "so", "as", "during",
    ]
    .into_iter()
    .collect()
});

/// Extract content words from a query by removing stop words and short tokens.
///
/// Returns `None` if the result is identical to the input (after normalisation)
/// or is too short to be useful as an additional search signal.
pub fn extract_content_words(query: &str) -> Option<String> {
    let words: Vec<&str> = query
        .split_whitespace()
        .filter(|w| {
            // Strip punctuation from both ends for comparison.
            let clean: String = w
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase();
            clean.len() >= 3 && !STOP_WORDS.contains(clean.as_str())
        })
        .collect();

    if words.len() < 2 {
        return None;
    }

    let result = words.join(" ");

    // Only return if it's meaningfully shorter than the original query —
    // i.e., at least one stop word was removed.
    let original_word_count = query.split_whitespace().count();
    if words.len() == original_word_count {
        return None;
    }

    Some(result)
}

#[derive(Debug, Clone, Serialize)]
pub struct SanitizeResult {
    pub clean_query: String,
    pub was_sanitized: bool,
    pub original_length: usize,
    pub clean_length: usize,
    pub method: SanitizeMethod,
}

#[derive(Debug, Clone, Serialize)]
pub enum SanitizeMethod {
    Passthrough,
    QuestionExtraction,
    TailSentence,
    TailTruncation,
}

/// Extract the actual search intent from a potentially contaminated query.
pub fn sanitize_query(raw: &str) -> SanitizeResult {
    let raw = raw.trim();

    if raw.is_empty() {
        return SanitizeResult {
            clean_query: String::new(),
            was_sanitized: false,
            original_length: 0,
            clean_length: 0,
            method: SanitizeMethod::Passthrough,
        };
    }

    let original_length = raw.len();

    // Step 1: Short query passthrough
    if original_length <= SAFE_QUERY_LENGTH {
        return SanitizeResult {
            clean_query: raw.to_string(),
            was_sanitized: false,
            original_length,
            clean_length: original_length,
            method: SanitizeMethod::Passthrough,
        };
    }

    // Split into segments by newlines
    let segments: Vec<&str> = raw
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // Step 2: Question extraction — find sentences ending with ?
    for seg in segments.iter().rev() {
        if QUESTION_MARK.is_match(seg) && seg.len() >= MIN_QUERY_LENGTH {
            let candidate = safe_tail(seg, MAX_QUERY_LENGTH);
            return SanitizeResult {
                clean_query: candidate.to_string(),
                was_sanitized: true,
                original_length,
                clean_length: candidate.len(),
                method: SanitizeMethod::QuestionExtraction,
            };
        }
    }

    // Step 3: Tail sentence extraction
    for seg in segments.iter().rev() {
        if seg.len() >= MIN_QUERY_LENGTH {
            let candidate = safe_tail(seg, MAX_QUERY_LENGTH);
            return SanitizeResult {
                clean_query: candidate.to_string(),
                was_sanitized: true,
                original_length,
                clean_length: candidate.len(),
                method: SanitizeMethod::TailSentence,
            };
        }
    }

    // Step 4: Tail truncation fallback
    let candidate = safe_tail(raw, MAX_QUERY_LENGTH).trim();

    SanitizeResult {
        clean_query: candidate.to_string(),
        was_sanitized: true,
        original_length,
        clean_length: candidate.len(),
        method: SanitizeMethod::TailTruncation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_passthrough() {
        let result = sanitize_query("What is chromadb?");
        assert!(!result.was_sanitized);
        assert!(matches!(result.method, SanitizeMethod::Passthrough));
    }

    #[test]
    fn test_question_extraction() {
        let long = format!(
            "{}\nWhat is the meaning of life?",
            "system prompt ".repeat(50)
        );
        let result = sanitize_query(&long);
        assert!(result.was_sanitized);
        assert_eq!(result.clean_query, "What is the meaning of life?");
        assert!(matches!(result.method, SanitizeMethod::QuestionExtraction));
    }

    #[test]
    fn test_empty_query() {
        let result = sanitize_query("");
        assert!(!result.was_sanitized);
        assert!(result.clean_query.is_empty());
    }

    #[test]
    fn test_multibyte_long_query_does_not_panic() {
        // 200 CJK chars = 600 bytes, exceeds MAX_QUERY_LENGTH (500)
        let cjk: String = "你好世界测试".repeat(34);
        assert!(cjk.len() > MAX_QUERY_LENGTH);
        let result = sanitize_query(&cjk);
        assert!(result.was_sanitized);
        assert!(result.clean_query.len() <= MAX_QUERY_LENGTH + 3); // +3 for char boundary
    }
}
