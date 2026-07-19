//! Shared error types used across CLI, MCP, and storage boundaries.

/// All error types for the `ironmem` crate.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("Embedding error: {0}")]
    Embed(#[from] anyhow::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Permission denied: {0}")]
    Permission(String),

    #[error("Migration error: {0}")]
    Migration(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Lock error: {0}")]
    Lock(String),

    /// Server-readiness precondition unmet: either readiness resolved to a
    /// failed terminal state, or a bounded wait for readiness timed out.
    /// No existing variant fits this "resource temporarily unavailable"
    /// semantic — `Lock` denotes mutex poisoning, `Config` denotes bad
    /// configuration, `Validation` denotes bad input — so this is a small,
    /// dedicated addition rather than an overload of an unrelated variant.
    #[error("Not ready: {0}")]
    NotReady(String),
}
