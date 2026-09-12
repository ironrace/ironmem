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

/// [`read_to_string_with_path`], with a size cap enforced *before* the read
/// and a leading UTF-8 BOM stripped after it. `kind` names what is being read
/// ("build manifest", "CI workflow") so the refusal message tells a human
/// which limit they hit.
///
/// Both halves exist because of the same class of bug. `read_to_string` has
/// no size limit of its own, so a path that resolves — directly, or through a
/// symlink a monorepo might legitimately use — to an unexpectedly large
/// regular file would be loaded into memory in full, and one that resolves to
/// something other than a regular file has no meaningful length to bound at
/// all. And a BOM (U+FEFF) is
/// not Unicode whitespace, so it survives every caller's trimming and hides
/// the very first line's real content: a `[workspace]` header, the opening
/// `{` of a JSON manifest (`serde_json` does not skip a BOM either), or a
/// workflow's first `run:` key. Stripping it once at the shared read boundary
/// is what keeps the format-specific parsers from each needing their own.
pub(crate) fn read_to_string_capped(
    path: &Path,
    max_bytes: u64,
    kind: &str,
) -> Result<String, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
    // The cap bounds a *regular file's* bytes. A character device reports
    // `len() == 0`, sails past the check, and then reads unbounded or blocks
    // forever — so the type check belongs here, with the guarantee this
    // function's doc advertises, rather than in each caller that happens to
    // have listed the path already.
    if !metadata.is_file() {
        return Err(format!(
            "'{}' is not a regular file — refusing to read it",
            path.display()
        ));
    }
    let size = metadata.len();
    if size > max_bytes {
        return Err(format!(
            "'{}' is {size} bytes, over the {max_bytes}-byte limit for a {kind} — refusing to \
             read it",
            path.display()
        ));
    }
    let content = read_to_string_with_path(path)?;
    Ok(content
        .strip_prefix('\u{FEFF}')
        .map(str::to_string)
        .unwrap_or(content))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capped_read_refuses_anything_that_is_not_a_regular_file() {
        // The cap bounds a regular file's bytes; a directory or a character
        // device has no length to bound, and a caller that skipped the check
        // would read unbounded. Both callers happen to pre-screen today, so
        // without this the guard is never executed by any test.
        let dir = tempfile::tempdir().unwrap();

        let err = read_to_string_capped(dir.path(), 1024, "build manifest").unwrap_err();

        assert!(err.contains("not a regular file"), "got: {err}");
    }

    #[test]
    fn a_capped_read_refuses_a_file_over_the_limit_and_names_both_it_and_the_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.toml");
        std::fs::write(&path, "x".repeat(64)).unwrap();

        let err = read_to_string_capped(&path, 8, "build manifest").unwrap_err();

        assert!(
            err.contains("big.toml") && err.contains("build manifest"),
            "got: {err}"
        );
    }

    #[test]
    fn a_capped_read_strips_a_leading_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.toml");
        std::fs::write(&path, "\u{FEFF}[workspace]\n").unwrap();

        let content = read_to_string_capped(&path, 1024, "build manifest").unwrap();

        assert!(content.starts_with("[workspace]"), "got: {content:?}");
    }
}
