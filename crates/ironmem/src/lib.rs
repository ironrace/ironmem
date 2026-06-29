//! `ironmem` is the workspace's main public crate: a local-first AI
//! memory backend with an MCP server, SQLite storage, semantic search, and
//! knowledge-graph utilities.

/// Background startup orchestration and stale-lock recovery.
pub mod bootstrap;
/// Pure bounded planning protocol and SQLite-backed queue helpers.
pub mod collab;
/// Configuration loading and environment-variable overrides.
pub mod config;
/// Task context packs — bounded memory/decisions/code-map view (issue #144).
pub mod context;
/// SQLite-backed persistence for drawers, WAL events, and graph state.
pub mod db;
/// Durable diary entry APIs layered on the shared memory store.
pub mod diary;
/// Shared error types returned across the crate.
pub mod error;
/// Hook entrypoints for Codex and Claude Code session lifecycle events.
pub mod hook;
/// Workspace mining and incremental re-indexing.
pub mod ingest;
/// MCP application state, protocol types, server loop, and tool dispatch.
pub mod mcp;
/// Pure metrics helpers shared by the MCP server and lifecycle hooks.
pub mod metrics;
/// Migration helpers for importing legacy Chroma-backed stores.
pub mod migrate;
/// Re-embedding all drawers after a model upgrade.
pub mod reembed;
/// `ironmem report` rendering layer (METRICS_SPEC §10 queries + §7 cost).
pub mod report;
/// Input sanitization helpers for names, content, harness IDs, and paths.
pub mod sanitize;
/// Search pipeline, graph traversal, and query sanitization.
pub mod search;

/// Lazy per-area code maps — freshness classification (issue #94).
pub mod code_maps;

/// Local symbol/import graph index — offline code-aware retrieval (migration 012).
/// v0: Rust + Python only, regex/heuristic parsers, no network.
pub mod symbol_graph;

/// `ironmem doctor` — local setup diagnostics (issue #142).
pub mod doctor;

/// One-command launchers for `claude` and `codex` (issue #143).
pub mod launcher;

pub mod write_rules;

/// Local read-only dashboard HTTP server (issue #165).
pub mod dashboard;

/// Extensible harness registry — canonical specs for Claude Code, Codex, and
/// future harnesses (issue #155).
pub mod harness;

/// Canonical crate error type used by CLI, MCP, and storage layers.
pub use error::MemoryError;
