//! Cross-encoder scoring trait + a deterministic test fixture.

use std::fmt;

use anyhow::Result;

use crate::response::LlmResponse;

/// Result of one rerank scoring call: the per-passage scores plus the optional
/// `LlmResponse` produced by an LLM-backed scorer (None for non-LLM scorers).
#[derive(Debug, Clone)]
pub struct RerankScoreResult {
    pub scores: Vec<f32>,
    pub llm_response: Option<LlmResponse>,
}

/// Scoring error for cases where an LLM call succeeded but its answer could not
/// be converted into scores. Carries the response so callers can still persist
/// token usage for the real call that already happened.
#[derive(Debug)]
pub struct RerankScoreError {
    message: String,
    pub llm_response: Option<LlmResponse>,
}

impl RerankScoreError {
    pub fn with_response(message: impl Into<String>, llm_response: LlmResponse) -> Self {
        Self {
            message: message.into(),
            llm_response: Some(llm_response),
        }
    }
}

impl fmt::Display for RerankScoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RerankScoreError {}

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
