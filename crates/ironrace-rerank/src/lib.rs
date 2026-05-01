//! ironrace-rerank: LLM-based reranker.
//!
//! Exposes the `RerankerScorer` trait (implementations: `LlmReranker` backed by
//! a `LlmClient`, plus a test-only `NoopScorer`). Production wiring uses
//! `ClaudeCliClient`; tests use `MockLlmClient`.

pub mod llm_client;
pub mod llm_reranker;
pub mod scorer;

pub use llm_client::{AnthropicApiClient, ClaudeCliClient, LlmClient, MockLlmClient};
pub use llm_reranker::LlmReranker;
pub use scorer::{NoopScorer, RerankerScorer};
