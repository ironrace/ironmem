//! `ironmem write-rules` — stamp the canonical memory protocol into rules files.
//!
//! Writes an idempotent, marker-delimited managed block sourced solely from
//! [`crate::bootstrap::MEMORY_PROTOCOL`]. Explicit opt-in only: no hook,
//! bootstrap, or serve path ever calls this.

use std::path::Path;

use crate::error::MemoryError;

const BEGIN_MARKER: &str = "<!-- BEGIN IRONMEM MEMORY PROTOCOL -->";
const END_MARKER: &str = "<!-- END IRONMEM MEMORY PROTOCOL -->";
const NOTICE: &str =
    "<!-- Managed by `ironmem write-rules`. Do not edit between these markers. -->";

/// Outcome of a single `write_rules_file` call, for CLI reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Updated,
    Unchanged,
}

/// Render the managed block: BEGIN marker, notice, protocol, END marker — each
/// on its own line (LF), with a single trailing newline. Deterministic.
pub fn render_block(protocol: &str) -> String {
    format!("{BEGIN_MARKER}\n{NOTICE}\n{protocol}\n{END_MARKER}\n")
}

/// Insert or replace the managed block in `existing`, returning the new content.
///
/// - No markers: append `block`, separated from non-empty content by exactly one
///   blank line. Empty/whitespace-only input returns `block` alone.
/// - Exactly one well-formed block (one BEGIN, one END, BEGIN before END):
///   replace the block (and its single trailing newline) in place, preserving
///   everything before and after byte-for-byte.
/// - Anything else (one marker only, END before BEGIN, duplicate pairs): return
///   an error so the caller can leave existing content untouched.
pub fn upsert_block(existing: &str, block: &str) -> Result<String, MemoryError> {
    let begins = existing.matches(BEGIN_MARKER).count();
    let ends = existing.matches(END_MARKER).count();

    match (begins, ends) {
        (0, 0) => Ok(append_block(existing, block)),
        (1, 1) => {
            let begin_idx = existing
                .find(BEGIN_MARKER)
                .expect("counted exactly one BEGIN");
            let end_idx = existing.find(END_MARKER).expect("counted exactly one END");
            if begin_idx > end_idx {
                return Err(MemoryError::Validation(
                    "ironmem managed block is malformed: END marker precedes BEGIN marker".into(),
                ));
            }
            let after_end = end_idx + END_MARKER.len();
            // Consume one trailing newline after END so replacement stays idempotent.
            let region_end = if existing[after_end..].starts_with('\n') {
                after_end + 1
            } else {
                after_end
            };
            let mut result = String::with_capacity(existing.len() + block.len());
            result.push_str(&existing[..begin_idx]);
            result.push_str(block);
            result.push_str(&existing[region_end..]);
            Ok(result)
        }
        _ => Err(MemoryError::Validation(format!(
            "ironmem managed block is malformed: found {begins} BEGIN and {ends} END markers \
             (expected exactly one of each)"
        ))),
    }
}

fn append_block(existing: &str, block: &str) -> String {
    if existing.trim().is_empty() {
        return block.to_string();
    }
    let trimmed = existing.trim_end_matches('\n');
    format!("{trimmed}\n\n{block}")
}

/// Read `target_path` (missing = empty), upsert the block rendered from
/// `protocol`, and write atomically. Skips the write when the result is
/// byte-identical to the existing file.
pub fn write_rules_file(target_path: &Path, protocol: &str) -> Result<WriteOutcome, MemoryError> {
    let existing = match std::fs::read_to_string(target_path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(MemoryError::Io(error)),
    };
    let existed = existing.is_some();
    let current = existing.unwrap_or_default();

    let block = render_block(protocol);
    let updated = upsert_block(&current, &block)?;

    if existed && updated == current {
        return Ok(WriteOutcome::Unchanged);
    }
    write_atomic(target_path, &updated)?;
    Ok(if existed {
        WriteOutcome::Updated
    } else {
        WriteOutcome::Created
    })
}

fn write_atomic(path: &Path, content: &str) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp_path = temp_path_for(path);
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

fn temp_path_for(path: &Path) -> std::path::PathBuf {
    let unique = format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("rules"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    path.with_file_name(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROTO: &str = "Always check memory first. Write durable summaries after.";

    #[test]
    fn render_block_is_deterministic_and_contains_protocol() {
        let a = render_block(PROTO);
        let b = render_block(PROTO);
        assert_eq!(a, b, "render must be deterministic");
        assert!(a.contains(PROTO), "block must contain the protocol text");
        assert!(a.starts_with(BEGIN_MARKER));
        assert!(a.trim_end().ends_with(END_MARKER));
        assert!(a.ends_with('\n'), "block ends with a trailing newline");
    }

    #[test]
    fn upsert_into_empty_yields_exactly_the_block() {
        let block = render_block(PROTO);
        let out = upsert_block("", &block).unwrap();
        assert_eq!(out, block);
    }

    #[test]
    fn upsert_whitespace_only_yields_exactly_the_block() {
        let block = render_block(PROTO);
        let out = upsert_block("   \n\n", &block).unwrap();
        assert_eq!(out, block);
    }

    #[test]
    fn upsert_appends_after_user_content_with_one_blank_line() {
        let block = render_block(PROTO);
        let out = upsert_block("# My rules\n", &block).unwrap();
        assert_eq!(out, format!("# My rules\n\n{block}"));
    }

    #[test]
    fn upsert_replaces_stale_block_byte_identical_to_fresh() {
        let block = render_block(PROTO);
        let stale = format!("{BEGIN_MARKER}\n{NOTICE}\nOLD STALE PROTOCOL\n{END_MARKER}\n");
        let out = upsert_block(&stale, &block).unwrap();
        assert_eq!(
            out, block,
            "stale block must be replaced with the fresh block"
        );
    }

    #[test]
    fn upsert_preserves_content_above_and_below_block() {
        let block = render_block(PROTO);
        let existing = format!("ABOVE LINE\n\n{BEGIN_MARKER}\nOLD\n{END_MARKER}\n\nBELOW LINE\n");
        let out = upsert_block(&existing, &block).unwrap();
        assert_eq!(out, format!("ABOVE LINE\n\n{block}\nBELOW LINE\n"));
        assert!(out.starts_with("ABOVE LINE\n\n"));
        assert!(out.ends_with("\nBELOW LINE\n"));
    }

    #[test]
    fn upsert_is_idempotent_byte_exact() {
        let block = render_block(PROTO);
        let once = upsert_block("# rules\n", &block).unwrap();
        let twice = upsert_block(&once, &block).unwrap();
        assert_eq!(once, twice, "second upsert must be byte-identical");
    }

    #[test]
    fn upsert_errors_on_begin_only() {
        let block = render_block(PROTO);
        let existing = format!("{BEGIN_MARKER}\nno end here\n");
        let err = upsert_block(&existing, &block).unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)));
    }

    #[test]
    fn upsert_errors_on_end_only() {
        let block = render_block(PROTO);
        let existing = format!("no begin here\n{END_MARKER}\n");
        assert!(upsert_block(&existing, &block).is_err());
    }

    #[test]
    fn upsert_errors_on_end_before_begin() {
        let block = render_block(PROTO);
        let existing = format!("{END_MARKER}\nmiddle\n{BEGIN_MARKER}\n");
        assert!(upsert_block(&existing, &block).is_err());
    }

    #[test]
    fn upsert_errors_on_duplicate_pairs() {
        let block = render_block(PROTO);
        let existing =
            format!("{BEGIN_MARKER}\nA\n{END_MARKER}\n{BEGIN_MARKER}\nB\n{END_MARKER}\n");
        assert!(upsert_block(&existing, &block).is_err());
    }

    #[test]
    fn write_rules_file_create_then_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CLAUDE.md");
        let first = write_rules_file(&path, PROTO).unwrap();
        assert_eq!(first, WriteOutcome::Created);
        let bytes1 = std::fs::read(&path).unwrap();
        let second = write_rules_file(&path, PROTO).unwrap();
        assert_eq!(second, WriteOutcome::Unchanged);
        let bytes2 = std::fs::read(&path).unwrap();
        assert_eq!(bytes1, bytes2, "second write must be byte-identical");
    }

    #[test]
    fn write_rules_file_updates_existing_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# Existing\n").unwrap();
        let outcome = write_rules_file(&path, PROTO).unwrap();
        assert_eq!(outcome, WriteOutcome::Updated);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("# Existing\n\n"));
        assert!(content.contains(BEGIN_MARKER));
    }

    #[test]
    fn write_rules_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("CLAUDE.md");
        let outcome = write_rules_file(&path, PROTO).unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert!(path.exists());
    }
}
