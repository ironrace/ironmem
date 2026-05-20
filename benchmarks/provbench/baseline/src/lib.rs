//! Phase 0c LLM-as-invalidator baseline runner for ProvBench-CodeContext.
//!
//! Three CLI subcommands: `sample` (stratified manifest), `run` (Anthropic
//! batch dispatch + checkpoint), `score` (§7.1 + §9.2 metrics).
//!
//! Benchmark scaffolding only — workspace-excluded; not imported by ironmem.
//! Frozen contract: `../SPEC.md`. Edits to anything frozen by the SPEC
//! (prompt text, model pin, batching, scoring) must be matched by a SPEC §11
//! entry and a new freeze hash.

pub mod budget;
pub mod client;
pub mod constants;
pub mod diffs;
pub mod facts;
pub mod manifest;
pub mod metrics;
pub mod prompt;
pub mod report;
pub mod runner;
pub mod sample;
pub mod throttle;
