//! End-to-end parse tests at integration-test scope (separate from inline
//! `#[cfg(test)] mod tests` in llm_reranker.rs).
//!
//! Contract under test: the reranker uses mempalace's "pick one" recipe — the
//! LLM returns a single 1-indexed integer naming the best passage; the
//! reranker promotes that passage to score 0.0 and assigns strictly-decreasing
//! scores `-(i+1)` to the rest by their ORIGINAL index, so a downstream sort
//! by `score DESC, idx ASC` floats the chosen passage to the top while
//! preserving the relative order of non-chosen passages.

use ironrace_rerank::{LlmReranker, MockLlmClient, RerankerScorer};

#[test]
fn integration_pick_one_score_encoding_via_mock() {
    // 5 passages; LLM picks passage 4 (1-indexed) → idx 3 promoted to top.
    let client = MockLlmClient::ok("4");
    let r = LlmReranker::new(client);
    let scores = r.score_pairs("q", &["a", "b", "c", "d", "e"]).unwrap();
    // Chosen idx 3 → 0.0; others → -(i+1) keeping original-index order.
    assert_eq!(scores, vec![-1.0, -2.0, -3.0, 0.0, -5.0]);
}

#[test]
fn integration_claude_envelope_round_trips() {
    // Envelope shape produced by `claude -p --output-format json`. The bare
    // integer in `result` is the chosen passage (1-indexed). Two passages,
    // pick the second one.
    let envelope = r#"{"type":"result","result":"2"}"#;
    let client = MockLlmClient::ok(envelope);
    let r = LlmReranker::new(client);
    let scores = r.score_pairs("q", &["a", "b"]).unwrap();
    // Chosen idx 1 → 0.0; idx 0 → -1.0.
    assert_eq!(scores, vec![-1.0, 0.0]);
}
