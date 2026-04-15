//! ironrace-embed: ONNX sentence embeddings (pure Rust).
//!
//! Provides `Embedder` for ONNX-based sentence embeddings using MiniLM-L6-v2.

/// ONNX model loading, caching, validation, and embedding APIs.
pub mod embedder;

/// Sentence embedder facade over the ONNX runtime or noop test mode.
pub use embedder::Embedder;
/// Output embedding dimensionality for the bundled MiniLM model.
pub use embedder::EMBED_DIM;
