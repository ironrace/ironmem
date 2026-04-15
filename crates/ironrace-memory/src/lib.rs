//! `ironrace-memory` is the workspace's main public crate: a local-first AI
//! memory backend with an MCP server, SQLite storage, semantic search, and
//! knowledge-graph utilities.

/// Background startup orchestration and stale-lock recovery.
pub mod bootstrap;
/// Configuration loading and environment-variable overrides.
pub mod config;
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
/// Migration helpers for importing legacy Chroma-backed stores.
pub mod migrate;
/// Input sanitization helpers for names, content, harness IDs, and paths.
pub mod sanitize;
/// Search pipeline, graph traversal, and query sanitization.
pub mod search;

/// Canonical crate error type used by CLI, MCP, and storage layers.
pub use error::MemoryError;
