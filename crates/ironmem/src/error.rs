//! Shared error types used across CLI, MCP, and storage boundaries.

use std::path::Path;

/// `std::fs::read_to_string`, but with the failing path folded into the
/// returned message — `std::io::Error`'s own `Display` never includes the
/// path it was operating on, which would otherwise leave a human debugging a
/// bare "Permission denied (os error 13)" with no indication of which file
/// caused it. Returns a plain `String` rather than a [`MemoryError`] because
/// callers wrap it into different variants depending on domain (a bad build
/// manifest is `Validation`, a bad IDE/CLI config file is `Config`) — this
/// is the one read+wrap idiom shared across those call sites, not the
/// decision of which variant it becomes.
pub(crate) fn read_to_string_with_path(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))
}

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
