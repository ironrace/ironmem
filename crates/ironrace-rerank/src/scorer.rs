//! Cross-encoder scoring trait + a deterministic test fixture.

use anyhow::Result;

/// Cross-encoder rerank interface.
///
/// Implementations score `(query, passage)` pairs and return one logit per
/// passage. Higher = more relevant. Raw logits are fine — callers only use
/// relative order.
pub trait RerankerScorer: Send + Sync {
    fn score_pairs(&self, query: &str, passages: &[&str]) -> Result<Vec<f32>>;
}

/// Test fixture: returns one zero per passage.
///
/// Used in `ironrace-rerank`'s own unit tests and as a passthrough fake when
/// `ironmem` integration tests need a non-erroring scorer that doesn't change
/// the candidate order.
#[derive(Default)]
pub struct NoopScorer;

impl NoopScorer {
    pub fn new() -> Self {
        Self
    }
}

impl RerankerScorer for NoopScorer {
    fn score_pairs(&self, _query: &str, passages: &[&str]) -> Result<Vec<f32>> {
        Ok(vec![0.0; passages.len()])
    }
}
