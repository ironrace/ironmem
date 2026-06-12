//! Cross-encoder scoring trait + a deterministic test fixture.

use anyhow::Result;

use crate::response::LlmResponse;

/// Result of one rerank scoring call: the per-passage scores plus the optional
/// `LlmResponse` produced by an LLM-backed scorer (None for non-LLM scorers).
#[derive(Debug, Clone)]
pub struct RerankScoreResult {
    pub scores: Vec<f32>,
    pub llm_response: Option<LlmResponse>,
}

/// Cross-encoder rerank interface.
///
/// Implementations score `(query, passage)` pairs and return one logit per
/// passage. Higher = more relevant. Raw logits are fine — callers only use
/// relative order.
pub trait RerankerScorer: Send + Sync {
    fn score_pairs(&self, query: &str, passages: &[&str]) -> Result<RerankScoreResult>;
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
    fn score_pairs(&self, _query: &str, passages: &[&str]) -> Result<RerankScoreResult> {
        Ok(RerankScoreResult {
            scores: vec![0.0; passages.len()],
            llm_response: None,
        })
    }
}
