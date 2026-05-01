//! Preference extractor for conversational text.
//!
//! Ported from mempalace's V4 regex set
//! (`mempalace/benchmarks/longmemeval_bench.py:1587-1610`). Pure CPU-bound,
//! deterministic, zero I/O. Intended to be called once per drawer at ingest
//! time when the content looks like a conversation.

mod patterns;

/// Strategy for extracting preference phrases from conversational text.
pub trait PreferenceExtractor: Send + Sync {
    /// Return up to N short phrases that describe user preferences,
    /// concerns, ongoing struggles, or memories. Order is the order of
    /// first occurrence in the input. Empty when the input has no matches.
    fn extract(&self, text: &str) -> Vec<String>;
}

/// Default implementation: a fixed set of V4 regexes scanned over the input.
#[derive(Debug, Default, Clone, Copy)]
pub struct RegexPreferenceExtractor;

impl PreferenceExtractor for RegexPreferenceExtractor {
    fn extract(&self, text: &str) -> Vec<String> {
        patterns::extract_v4(text)
    }
}

/// Cheap structural test: does the text contain a first-person pronoun in
/// the first 500 chars? Intended as a guard so we don't run the regex set
/// on file chunks or non-conversational mining input.
pub fn looks_conversational(text: &str) -> bool {
    let head: String = text.chars().take(500).collect::<String>().to_lowercase();
    const NEEDLES: &[&str] = &[" i ", " i'", "i've ", "i'm ", " my ", " me "];
    if head.starts_with("i ") || head.starts_with("i'") {
        return true;
    }
    NEEDLES.iter().any(|n| head.contains(n))
}

/// Build the synthetic doc string from extracted phrases. Returns `None`
/// when there are no phrases (caller should skip the sibling insert).
///
/// Format: phrases joined by `". "` with no meta prefix. The bare-phrase
/// format keeps the synthetic embedding closer to query embeddings on
/// `bge-base-en-v1.5`; the earlier `"User has mentioned: "` prefix
/// dominated the embedding signal and prevented preference R@5 lift.
pub fn synthesize_doc(phrases: &[String]) -> Option<String> {
    if phrases.is_empty() {
        return None;
    }
    Some(phrases.join(". "))
}
