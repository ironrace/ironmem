//! LLM-based reranker. Implements `RerankerScorer` by calling out to an
//! `LlmClient` (production: Claude Haiku via `claude` CLI).
//!
//! Recipe: mempalace "pick one". The LLM is asked to choose the SINGLE
//! candidate that best answers the question (1..N), not to produce a full
//! ranking. Empirically this is a much easier task for small/fast models
//! than rank-all, especially with reasoning-style decoders. See:
//! benchmarks/locomo_bench.py:512 in mempalace for the reference prompt.
//!
//! Scoring contract (consumed by `crate::scorer::RerankerScorer`):
//!   * The chosen passage gets the maximum score (`0.0`).
//!   * All other passages get strictly-decreasing scores by their ORIGINAL
//!     index (`-(i+1)` for i ≠ chosen). This preserves the pre-rerank
//!     order of non-chosen items after the post-rerank re-sort, which uses
//!     `drawer_id` as a tiebreaker — bare equal scores would NOT preserve
//!     order under that tiebreak.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::llm_client::LlmClient;
use crate::scorer::RerankerScorer;

/// Pinned rerank prompt template ("pick one" / mempalace recipe).
/// Changes require a full eval re-run.
const RERANK_PROMPT_TEMPLATE: &str = "Question: {QUERY}\n\nWhich of the following passages most directly answers this question? Reply with just the number (1-{N}).\n\n{CANDIDATES}";

/// Truncate each passage to this many chars before sending. Mempalace uses
/// 300, but their candidates are message-level chunks; ours are full sessions
/// (multi-thousand chars), so 300 only reaches the session opener and the
/// answer span is past the cut. 2000 lets the LLM see most of a session and
/// is the only setting that produced an R@1 lift in the 50-q probe (+10pp).
const PASSAGE_MAX_CHARS: usize = 2000;

pub struct LlmReranker<C: LlmClient> {
    client: C,
}

impl<C: LlmClient> LlmReranker<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }
}

impl<C: LlmClient> RerankerScorer for LlmReranker<C> {
    fn score_pairs(&self, query: &str, passages: &[&str]) -> Result<Vec<f32>> {
        if passages.is_empty() {
            return Ok(Vec::new());
        }
        let prompt = build_rerank_prompt(query, passages);
        let raw = self
            .client
            .call(&prompt)
            .context("LLM client call failed")?;
        let chosen = parse_chosen_index(&raw, passages.len())?;

        // Chosen → 0.0, others → -(i+1) so the post-rerank sort preserves
        // original order for non-chosen items.
        let mut scores: Vec<f32> = (0..passages.len()).map(|i| -((i + 1) as f32)).collect();
        scores[chosen] = 0.0;
        Ok(scores)
    }
}

fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn build_rerank_prompt(query: &str, passages: &[&str]) -> String {
    let n = passages.len();
    let mut candidates = String::new();
    for (i, p) in passages.iter().enumerate() {
        // 1-indexed numbering, mempalace style.
        candidates.push_str(&format!(
            "{}. {}\n",
            i + 1,
            truncate_chars(p, PASSAGE_MAX_CHARS)
        ));
    }
    RERANK_PROMPT_TEMPLATE
        .replace("{QUERY}", query)
        .replace("{N}", &n.to_string())
        .replace("{CANDIDATES}", &candidates)
}

/// Parse the LLM's response into a 0-indexed chosen passage index.
/// Strategy: unwrap any `claude -p` JSON envelope, then take the LAST integer
/// in the assistant text (handles reasoning-style decoders that show work
/// before the final answer). The integer is interpreted as 1-indexed and
/// clamped to `[1, expected_n]`.
fn parse_chosen_index(raw: &str, expected_n: usize) -> Result<usize> {
    let assistant_text = extract_assistant_text(raw).unwrap_or_else(|| raw.to_string());
    let last = last_integer(&assistant_text);
    match last {
        Some(n) if (1..=expected_n).contains(&n) => Ok(n - 1),
        Some(n) => {
            tracing::trace!(
                raw = %raw,
                assistant = %assistant_text,
                parsed = n,
                expected_n,
                "rerank response: chosen index out of range"
            );
            bail!("LLM returned chosen index {n} outside 1..={expected_n}")
        }
        None => {
            tracing::trace!(raw = %raw, assistant = %assistant_text, "rerank response: no integer found");
            bail!("could not parse a passage number from LLM response")
        }
    }
}

fn extract_assistant_text(raw: &str) -> Option<String> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if let Some(Value::String(s)) = v.get("result") {
        return Some(s.clone());
    }
    None
}

/// Return the last contiguous run of ASCII digits as a `usize`. Tolerant of
/// surrounding prose, punctuation, and multi-digit numbers in reasoning.
fn last_integer(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    // Skip trailing non-digits.
    while i > 0 && !bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let end = i;
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    // Safe: ASCII digit bytes are always single-byte UTF-8, so [i..end] is a valid str boundary.
    s[i..end].parse::<usize>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_client::MockLlmClient;

    #[test]
    fn parse_bare_integer() {
        assert_eq!(parse_chosen_index("3", 5).unwrap(), 2);
    }

    #[test]
    fn parse_claude_envelope() {
        let raw = r#"{"type":"result","result":"5"}"#;
        assert_eq!(parse_chosen_index(raw, 10).unwrap(), 4);
    }

    #[test]
    fn parse_envelope_with_trailing_whitespace() {
        let raw = r#"{"type":"result","result":" 7\n"}"#;
        assert_eq!(parse_chosen_index(raw, 10).unwrap(), 6);
    }

    #[test]
    fn parse_takes_last_integer_after_reasoning() {
        // Reasoning-style decoder that shows its work before answering.
        let raw = "Looking at the candidates, passage 3 mentions X and passage 7 directly answers. Answer: 7";
        assert_eq!(parse_chosen_index(raw, 10).unwrap(), 6);
    }

    #[test]
    fn parse_index_out_of_range_errors() {
        assert!(parse_chosen_index("99", 10).is_err());
        assert!(parse_chosen_index("0", 10).is_err());
    }

    #[test]
    fn parse_no_integer_errors() {
        assert!(parse_chosen_index("I don't know", 10).is_err());
    }

    #[test]
    fn last_integer_basic() {
        assert_eq!(last_integer("42"), Some(42));
        assert_eq!(last_integer("answer is 7"), Some(7));
        assert_eq!(last_integer("1 then 2 then 3"), Some(3));
        assert_eq!(last_integer("none"), None);
        assert_eq!(last_integer(""), None);
        assert_eq!(last_integer("12 abc"), Some(12));
    }

    #[test]
    fn build_prompt_uses_1_indexed_numbering() {
        let p = build_rerank_prompt("q", &["alpha", "beta"]);
        assert!(p.contains("1. alpha"));
        assert!(p.contains("2. beta"));
        assert!(p.contains("(1-2)"));
        assert!(p.contains("Question: q"));
    }

    #[test]
    fn build_prompt_truncates_long_passages() {
        // Input must exceed PASSAGE_MAX_CHARS for truncation to be observable.
        let long = "x".repeat(PASSAGE_MAX_CHARS * 2);
        let p = build_rerank_prompt("q", &[&long]);
        assert!(p.matches('x').count() == PASSAGE_MAX_CHARS);
    }

    #[test]
    fn score_pairs_promotes_chosen_to_top() {
        // LLM picks passage 3 (1-indexed) → idx 2.
        let client = MockLlmClient::ok("3");
        let r = LlmReranker::new(client);
        let scores = r.score_pairs("q", &["a", "b", "c", "d"]).unwrap();
        // Chosen has highest score.
        assert_eq!(scores[2], 0.0);
        // Non-chosen are strictly decreasing in original index.
        assert!(scores[0] > scores[1]);
        assert!(scores[1] > scores[3]);
        // Sanity: max is uniquely the chosen.
        let max_idx = scores
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(max_idx, 2);
    }

    #[test]
    fn score_pairs_preserves_non_chosen_order_after_resort() {
        // Mimic the pipeline's post-rerank sort: score DESC, idx ASC tiebreak.
        let client = MockLlmClient::ok("2");
        let r = LlmReranker::new(client);
        let passages = ["a", "b", "c", "d", "e"];
        let scores = r.score_pairs("q", &passages).unwrap();

        let mut order: Vec<usize> = (0..passages.len()).collect();
        order.sort_by(|&i, &j| {
            scores[j]
                .partial_cmp(&scores[i])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| i.cmp(&j))
        });
        // Chosen idx 1 floats to top, rest stay in [0, 2, 3, 4].
        assert_eq!(order, vec![1, 0, 2, 3, 4]);
    }

    #[test]
    fn score_pairs_envelope_response() {
        let client = MockLlmClient::ok(r#"{"type":"result","result":"1"}"#);
        let r = LlmReranker::new(client);
        let scores = r.score_pairs("q", &["a", "b", "c"]).unwrap();
        assert_eq!(scores[0], 0.0);
    }

    #[test]
    fn score_pairs_empty() {
        let client = MockLlmClient::ok("");
        let r = LlmReranker::new(client);
        assert!(r.score_pairs("q", &[]).unwrap().is_empty());
    }

    #[test]
    fn score_pairs_unparseable_errors() {
        let client = MockLlmClient::ok("I cannot answer");
        let r = LlmReranker::new(client);
        assert!(r.score_pairs("q", &["a", "b"]).is_err());
    }

    #[test]
    fn score_pairs_out_of_range_errors() {
        let client = MockLlmClient::ok("99");
        let r = LlmReranker::new(client);
        assert!(r.score_pairs("q", &["a", "b"]).is_err());
    }

    #[test]
    fn score_pairs_client_error_propagates() {
        let client = MockLlmClient::err("network down");
        let r = LlmReranker::new(client);
        assert!(r.score_pairs("q", &["a"]).is_err());
    }
}
