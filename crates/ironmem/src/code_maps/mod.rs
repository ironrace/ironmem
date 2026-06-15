//! Lazy per-area code maps — freshness classification and module root (issue #94).
pub mod freshness;
pub use freshness::{classify, Freshness};
