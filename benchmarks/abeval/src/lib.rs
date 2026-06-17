//! abeval — A/B eval harness for METRICS_SPEC §11 arms.
//!
//! Standalone crate, excluded from the ironmem workspace (own Cargo.lock),
//! mirroring `benchmarks/provbench/baseline`.

pub mod arms;
pub mod client;
pub mod codex_tokens;
pub mod collab_db;
pub mod constants;
pub mod corpus;
pub mod report;
pub mod runner;
