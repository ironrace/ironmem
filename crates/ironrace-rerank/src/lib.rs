//! ironrace-rerank: ONNX cross-encoder reranker.
//!
//! Exposes the `RerankerScorer` trait (implementations: real ONNX `Reranker`
//! and test-only `NoopScorer`) plus the `Reranker` struct itself for callers
//! wiring it into a search pipeline.

pub mod scorer;
// `output` and `reranker` modules are added in Tasks 4 and 5 respectively.
