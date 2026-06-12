use ironrace_rerank::{NoopScorer, RerankerScorer};

#[test]
fn noop_returns_one_score_per_passage() {
    let s = NoopScorer::new();
    let out = s.score_pairs("query", &["a", "b", "c"]).unwrap();
    assert_eq!(out.scores.len(), 3);
}

#[test]
fn noop_is_deterministic() {
    let s = NoopScorer::new();
    let out1 = s.score_pairs("q", &["alpha", "beta"]).unwrap();
    let out2 = s.score_pairs("q", &["alpha", "beta"]).unwrap();
    assert_eq!(out1.scores, out2.scores);
}

#[test]
fn noop_handles_empty_passages() {
    let s = NoopScorer::new();
    let out = s.score_pairs("q", &[]).unwrap();
    assert!(out.scores.is_empty());
}
