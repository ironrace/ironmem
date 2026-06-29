//! `ironmem write-rules` — stamp the canonical memory protocol into rules files.
//!
//! Writes an idempotent, marker-delimited managed block sourced solely from
//! [`crate::bootstrap::MEMORY_PROTOCOL`]. Explicit opt-in only: no hook,
//! bootstrap, or serve path ever calls this.

use std::io::Write;
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
            // Consume one trailing line ending after END so replacement stays idempotent.
            let region_end = if existing[after_end..].starts_with("\r\n") {
                after_end + 2
            } else if existing[after_end..].starts_with('\n') {
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
    let trimmed = trim_trailing_line_endings(existing);
    let separator = if existing.contains("\r\n") {
        "\r\n\r\n"
    } else {
        "\n\n"
    };
    format!("{trimmed}{separator}{block}")
}

fn trim_trailing_line_endings(mut value: &str) -> &str {
    loop {
        if let Some(trimmed) = value.strip_suffix("\r\n") {
            value = trimmed;
        } else if let Some(trimmed) = value.strip_suffix('\n') {
            value = trimmed;
        } else {
            return value;
        }
    }
}

/// Resolve which rules files `write-rules` should target.
///
/// - Both `target` and `harness` `None` → all `default_rules_targets` (e.g. CLAUDE.md, AGENTS.md).
/// - `target` `Some` → validate it equals one of the registry `rules_file`s; error listing allowed
///   targets otherwise.
/// - `harness` `Some` → canonicalize/look up the harness (accepts both ids like `"codex"` and
///   env-aliases like `"claude-code"`); resolve to its `rules_file`; error if unknown harness.
///
/// `target` and `harness` are mutually exclusive — the caller (clap) enforces this.
pub fn resolve_write_targets(
    target: Option<&str>,
    harness: Option<&str>,
    registry: &[crate::harness::HarnessSpec],
) -> Result<Vec<&'static str>, MemoryError> {
    match (target, harness) {
        (None, None) => Ok(crate::harness::default_rules_targets(registry)),
        (Some(t), None) => {
            // Collect all rules_files from the registry (sorted, deduped for a stable message).
            let mut allowed: Vec<&'static str> = registry.iter().map(|s| s.rules_file).collect();
            allowed.sort_unstable();
            allowed.dedup();

            registry
                .iter()
                .find(|s| s.rules_file == t)
                .map(|s| vec![s.rules_file])
                .ok_or_else(|| {
                    MemoryError::Validation(format!(
                        "unknown target '{}': allowed targets are {}",
                        t,
                        allowed.join(", ")
                    ))
                })
        }
        (None, Some(h)) => {
            // Accept both an id (e.g. "codex") and an env-alias (e.g. "claude-code").
            let spec = crate::harness::by_id(h, registry).or_else(|| {
                crate::harness::canonicalize_input(h, registry)
                    .and_then(|id| crate::harness::by_id(id, registry))
            });
            spec.map(|s| vec![s.rules_file]).ok_or_else(|| {
                let mut known: Vec<&str> = registry.iter().map(|s| s.id).collect();
                known.sort_unstable();
                MemoryError::Validation(format!(
                    "unknown harness '{}': known harnesses are {}",
                    h,
                    known.join(", ")
                ))
            })
        }
        (Some(_), Some(_)) => {
            // clap's `conflicts_with` prevents this branch in production.
            Err(MemoryError::Validation(
                "--target and --harness are mutually exclusive".into(),
            ))
        }
    }
}

/// Read `target_path` (missing = empty), upsert the block rendered from
/// `protocol`, and write atomically. Skips the write when the result is
/// byte-identical to the existing file.
pub fn write_rules_file(target_path: &Path, protocol: &str) -> Result<WriteOutcome, MemoryError> {
    let (target_metadata, current) = read_rules_target(target_path)?;
    let existed = target_metadata.is_some();

    let block = render_block(protocol);
    let updated = upsert_block(&current, &block)?;

    if existed && updated == current {
        return Ok(WriteOutcome::Unchanged);
    }
    write_atomic(target_path, &updated, target_metadata.as_ref())?;
    Ok(if existed {
        WriteOutcome::Updated
    } else {
        WriteOutcome::Created
    })
}

/// Validate that `target_path` can be read and upserted without writing it.
pub fn validate_rules_file(target_path: &Path, protocol: &str) -> Result<(), MemoryError> {
    let (_target_metadata, current) = read_rules_target(target_path)?;
    let block = render_block(protocol);
    upsert_block(&current, &block)?;
    Ok(())
}

fn read_rules_target(
    target_path: &Path,
) -> Result<(Option<std::fs::Metadata>, String), MemoryError> {
    let target_metadata = match std::fs::symlink_metadata(target_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(MemoryError::Validation(format!(
                "ironmem write-rules refuses to overwrite symlink target {}",
                target_path.display()
            )));
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(MemoryError::Io(error)),
    };
    let current = if target_metadata.is_some() {
        std::fs::read_to_string(target_path)?
    } else {
        String::new()
    };
    Ok((target_metadata, current))
}

fn write_atomic(
    path: &Path,
    content: &str,
    existing_metadata: Option<&std::fs::Metadata>,
) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp_path = temp_path_for(path);
    let result = (|| -> Result<(), MemoryError> {
        let mut file = create_temp_file(&tmp_path, existing_metadata)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        replace_file(&tmp_path, path)?;
        set_final_permissions(path, existing_metadata)?;
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn create_temp_file(
    path: &Path,
    existing_metadata: Option<&std::fs::Metadata>,
) -> Result<std::fs::File, MemoryError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(target_mode(existing_metadata));
    }
    Ok(options.open(path)?)
}

#[cfg(not(windows))]
fn replace_file(tmp_path: &Path, path: &Path) -> Result<(), MemoryError> {
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(tmp_path: &Path, path: &Path) -> Result<(), MemoryError> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

#[cfg(unix)]
fn set_final_permissions(
    path: &Path,
    existing_metadata: Option<&std::fs::Metadata>,
) -> Result<(), MemoryError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(target_mode(existing_metadata)),
    )?;
    Ok(())
}

#[cfg(not(unix))]
fn set_final_permissions(
    _path: &Path,
    _existing_metadata: Option<&std::fs::Metadata>,
) -> Result<(), MemoryError> {
    Ok(())
}

#[cfg(unix)]
fn target_mode(existing_metadata: Option<&std::fs::Metadata>) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    existing_metadata
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o644)
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<(), MemoryError> {
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
    fn upsert_appends_after_crlf_user_content_with_crlf_separator() {
        let block = render_block(PROTO);
        let out = upsert_block("# My rules\r\n", &block).unwrap();
        assert_eq!(out, format!("# My rules\r\n\r\n{block}"));
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
    fn upsert_preserves_crlf_content_around_block() {
        let block = render_block(PROTO);
        let existing =
            format!("ABOVE LINE\r\n{BEGIN_MARKER}\r\nOLD\r\n{END_MARKER}\r\nBELOW LINE\r\n");
        let out = upsert_block(&existing, &block).unwrap();
        assert_eq!(out, format!("ABOVE LINE\r\n{block}BELOW LINE\r\n"));
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

    #[cfg(unix)]
    #[test]
    fn write_rules_file_preserves_existing_unix_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# Private rules\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let outcome = write_rules_file(&path, PROTO).unwrap();

        assert_eq!(outcome, WriteOutcome::Updated);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing file mode must be preserved");
    }

    // ---- resolve_write_targets --------------------------------------------

    use crate::harness::{HarnessSpec, TranscriptParserKind, REGISTRY};

    const GEMINI_SPEC: HarnessSpec = HarnessSpec {
        id: "gemini",
        display_name: "Gemini CLI",
        binary: "gemini",
        rules_file: "GEMINI.md",
        write_rules_default: false,
        client_info_aliases: &["gemini"],
        env_aliases: &["gemini"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    fn three_entry_registry() -> [HarnessSpec; 3] {
        [REGISTRY[0], REGISTRY[1], GEMINI_SPEC]
    }

    #[test]
    fn resolve_write_targets_no_args_returns_both_defaults() {
        let targets = resolve_write_targets(None, None, REGISTRY).unwrap();
        assert_eq!(targets, vec!["CLAUDE.md", "AGENTS.md"]);
    }

    #[test]
    fn resolve_write_targets_target_claude_md() {
        let targets = resolve_write_targets(Some("CLAUDE.md"), None, REGISTRY).unwrap();
        assert_eq!(targets, vec!["CLAUDE.md"]);
    }

    #[test]
    fn resolve_write_targets_target_agents_md() {
        let targets = resolve_write_targets(Some("AGENTS.md"), None, REGISTRY).unwrap();
        assert_eq!(targets, vec!["AGENTS.md"]);
    }

    #[test]
    fn resolve_write_targets_unknown_target_lists_allowed() {
        let err = resolve_write_targets(Some("FOO.md"), None, REGISTRY).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("CLAUDE.md"), "error should list CLAUDE.md");
        assert!(msg.contains("AGENTS.md"), "error should list AGENTS.md");
    }

    #[test]
    fn resolve_write_targets_harness_codex_by_id() {
        let targets = resolve_write_targets(None, Some("codex"), REGISTRY).unwrap();
        assert_eq!(targets, vec!["AGENTS.md"]);
    }

    #[test]
    fn resolve_write_targets_harness_claude_by_id() {
        let targets = resolve_write_targets(None, Some("claude"), REGISTRY).unwrap();
        assert_eq!(targets, vec!["CLAUDE.md"]);
    }

    #[test]
    fn resolve_write_targets_harness_claude_code_env_alias() {
        let targets = resolve_write_targets(None, Some("claude-code"), REGISTRY).unwrap();
        assert_eq!(targets, vec!["CLAUDE.md"]);
    }

    #[test]
    fn resolve_write_targets_unknown_harness_errors() {
        let err = resolve_write_targets(None, Some("gemini"), REGISTRY).unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("claude"),
            "error must list known harness id 'claude'; got: {msg}"
        );
        assert!(
            msg.contains("codex"),
            "error must list known harness id 'codex'; got: {msg}"
        );
    }

    #[test]
    fn resolve_write_targets_both_args_errors() {
        let err = resolve_write_targets(Some("CLAUDE.md"), Some("codex"), REGISTRY).unwrap_err();
        assert!(
            matches!(err, MemoryError::Validation(_)),
            "both --target and --harness must produce a Validation error; got: {err:?}"
        );
    }

    #[test]
    fn resolve_write_targets_synthetic_gemini_in_injected_registry() {
        let reg = three_entry_registry();
        let targets = resolve_write_targets(None, Some("gemini"), &reg).unwrap();
        assert_eq!(targets, vec!["GEMINI.md"]);
    }

    #[test]
    fn default_rules_targets_injected_excludes_non_default() {
        let reg = three_entry_registry();
        let targets = crate::harness::default_rules_targets(&reg);
        assert!(!targets.contains(&"GEMINI.md"));
        assert_eq!(targets, vec!["CLAUDE.md", "AGENTS.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn write_rules_file_rejects_symlink_targets_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.md");
        let link = dir.path().join("AGENTS.md");
        std::fs::write(&real, "# Real target\n").unwrap();
        symlink(&real, &link).unwrap();

        let err = write_rules_file(&link, PROTO).unwrap_err();

        assert!(matches!(err, MemoryError::Validation(_)));
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "# Real target\n",
            "symlink target content must be untouched"
        );
    }
}
