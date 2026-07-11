//! `ironmem write-rules` — stamp the canonical memory protocol into rules files.
//!
//! Writes idempotent, marker-delimited managed blocks. Only the canonical
//! `AGENTS.md` block is sourced from [`crate::bootstrap::MEMORY_PROTOCOL`];
//! dependent harness files receive a strategy-derived block instead — an
//! [`Import`](crate::harness::RulesStrategy::Import) directive such as
//! `@AGENTS.md`, or a flattened [`Copy`](crate::harness::RulesStrategy::Copy)
//! of the canonical block. Explicit opt-in only: no hook, bootstrap, or serve
//! path ever calls this.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::MemoryError;

const BEGIN_MARKER: &str = "<!-- BEGIN IRONMEM MEMORY PROTOCOL -->";
const END_MARKER: &str = "<!-- END IRONMEM MEMORY PROTOCOL -->";
const NOTICE: &str =
    "<!-- Managed by `ironmem write-rules`. Do not edit between these markers. -->";

/// Canonical rules file, re-exported from the harness registry so both layers
/// reference a single definition.
pub use crate::harness::CANONICAL_RULES_FILE;

/// Outcome of a single managed write, for CLI reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Updated,
    Unchanged,
}

/// A preflighted write plan entry ready for batched persistence.
#[derive(Debug, Clone)]
pub struct WritePlanItem {
    pub target_path: PathBuf,
    pub planned_contents: String,
    pub existing_metadata: Option<std::fs::Metadata>,
    pub existing_contents: String,
}

#[derive(Debug, Clone, Copy)]
struct StrategyTarget {
    rules_file: &'static str,
    rules_strategy: crate::harness::RulesStrategy,
}

/// Render a managed block: BEGIN marker, notice, contents, END marker — each on
/// its own line (LF), with a single trailing newline. Deterministic.
pub fn render_block(contents: &str) -> String {
    format!("{BEGIN_MARKER}\n{NOTICE}\n{contents}\n{END_MARKER}\n")
}

/// Insert or replace a managed block in `existing`, returning the resulting text.
///
/// - No markers: append `block`, separated from non-empty content by exactly one
///   blank line. Empty/whitespace-only input returns `block` alone.
/// - Exactly one well-formed block (one BEGIN, one END, BEGIN before END):
///   replace the block in place, consuming a trailing newline after END and
///   preserving everything else byte-for-byte.
/// - Anything else (one marker only, END before BEGIN, duplicate pairs): return
///   an error so the caller can leave existing content untouched.
pub fn upsert_block(existing: &str, block: &str) -> Result<String, MemoryError> {
    let region = managed_block_region(existing)?;

    match region {
        None => Ok(append_block(existing, block)),
        Some((begin_idx, _end_idx, region_end)) => {
            let mut result = String::with_capacity(existing.len() + block.len());
            result.push_str(&existing[..begin_idx]);
            result.push_str(block);
            result.push_str(&existing[region_end..]);
            Ok(result)
        }
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

/// Resolve strategy-bearing requested targets from `target` / `harness`.
fn resolve_strategy_targets(
    target: Option<&str>,
    harness: Option<&str>,
    registry: &[crate::harness::HarnessSpec],
) -> Result<Vec<StrategyTarget>, MemoryError> {
    // Validate strategy invariants and de-duplicate by file/strategy.
    let normalized =
        crate::harness::rules_file_entries(registry).map_err(MemoryError::Validation)?;

    let mut selected: Vec<StrategyTarget> = Vec::new();

    match (target, harness) {
        (None, None) => {
            for spec in registry.iter().filter(|spec| spec.write_rules_default) {
                if let Some(entry) = normalized
                    .iter()
                    .find(|entry| entry.rules_file == spec.rules_file)
                {
                    if !selected
                        .iter()
                        .any(|existing| existing.rules_file == entry.rules_file)
                    {
                        selected.push(StrategyTarget {
                            rules_file: entry.rules_file,
                            rules_strategy: entry.rules_strategy,
                        });
                    }
                }
            }
        }
        (Some(t), None) => {
            if let Some(entry) = normalized.iter().find(|entry| entry.rules_file == t) {
                selected.push(StrategyTarget {
                    rules_file: entry.rules_file,
                    rules_strategy: entry.rules_strategy,
                });
            } else {
                let mut allowed: Vec<&str> =
                    normalized.iter().map(|entry| entry.rules_file).collect();
                allowed.sort_unstable();
                allowed.dedup();
                return Err(MemoryError::Validation(format!(
                    "unknown target '{}': allowed targets are {}",
                    t,
                    allowed.join(", ")
                )));
            }
        }
        (None, Some(h)) => {
            let spec = crate::harness::by_id(h, registry).or_else(|| {
                crate::harness::canonicalize_input(h, registry)
                    .and_then(|id| crate::harness::by_id(id, registry))
            });
            let spec = spec.ok_or_else(|| {
                let mut known: Vec<&str> = registry.iter().map(|s| s.id).collect();
                known.sort_unstable();
                known.dedup();
                MemoryError::Validation(format!(
                    "unknown harness '{}': known harnesses are {}",
                    h,
                    known.join(", ")
                ))
            })?;

            if let Some(entry) = normalized
                .iter()
                .find(|entry| entry.rules_file == spec.rules_file)
            {
                selected.push(StrategyTarget {
                    rules_file: entry.rules_file,
                    rules_strategy: entry.rules_strategy,
                });
            }
        }
        (Some(_), Some(_)) => {
            return Err(MemoryError::Validation(
                "--target and --harness are mutually exclusive".into(),
            ));
        }
    }

    Ok(selected)
}

/// Resolve which files `write-rules` should target.
///
/// This legacy helper keeps existing CLI behavior: it returns deduplicated target
/// filenames and surfaces the same validation errors as the strategy-aware planner.
pub fn resolve_write_targets(
    target: Option<&str>,
    harness: Option<&str>,
    registry: &[crate::harness::HarnessSpec],
) -> Result<Vec<&'static str>, MemoryError> {
    Ok(resolve_strategy_targets(target, harness, registry)?
        .into_iter()
        .map(|entry| entry.rules_file)
        .collect())
}

/// Build an ordered, preflighted write plan.
///
/// When a canonical or non-native target is present, the canonical `AGENTS.md`
/// plan item is ordered first, followed by deduplicated dependent items in
/// target order. This builds the plan only; [`apply_write_rules_plan`] performs
/// the writes.
pub fn build_write_rules_plan(
    workspace: &Path,
    target: Option<&str>,
    harness: Option<&str>,
    registry: &[crate::harness::HarnessSpec],
) -> Result<Vec<WritePlanItem>, MemoryError> {
    let targets = resolve_strategy_targets(target, harness, registry)?;

    let need_canonical = targets
        .iter()
        .any(|entry| entry.rules_file == CANONICAL_RULES_FILE)
        || targets
            .iter()
            .any(|entry| entry.rules_strategy != crate::harness::RulesStrategy::Native);

    let mut plan = Vec::new();
    let mut projected_canonical = None;

    if need_canonical {
        let canonical_path = workspace.join(CANONICAL_RULES_FILE);
        let canonical_block = render_block(crate::bootstrap::MEMORY_PROTOCOL);
        let (canonical_metadata, canonical_existing) = read_rules_target(&canonical_path)?;
        let canonical_planned = upsert_block(&canonical_existing, &canonical_block)?;

        let canonical_entry = WritePlanItem {
            target_path: canonical_path,
            planned_contents: canonical_planned.clone(),
            existing_metadata: canonical_metadata,
            existing_contents: canonical_existing,
        };
        projected_canonical = Some(canonical_entry.planned_contents.clone());
        plan.push(canonical_entry);
    }

    for target in targets {
        if target.rules_file == CANONICAL_RULES_FILE {
            continue;
        }

        let dependency_contents = match target.rules_strategy {
            crate::harness::RulesStrategy::Native => {
                continue;
            }
            crate::harness::RulesStrategy::Import { directive } => directive.to_owned(),
            crate::harness::RulesStrategy::Copy => {
                let projected = projected_canonical.as_ref().ok_or_else(|| {
                    MemoryError::Validation("missing canonical projection for copy target".into())
                })?;
                flatten_and_rewrap_agent_canonical_rules(projected)?
            }
        };

        let target_path = workspace.join(target.rules_file);
        let (metadata, existing) = read_rules_target(&target_path)?;
        let rendered = render_block(&dependency_contents);
        let planned_contents = upsert_block(&existing, &rendered)?;

        plan.push(WritePlanItem {
            target_path,
            planned_contents,
            existing_metadata: metadata,
            existing_contents: existing,
        });
    }

    Ok(plan)
}

/// Apply a write plan and return the per-file outcomes for rendering.
///
/// Two phases: every target is re-read and re-validated first, and any drift
/// from the plan's captured snapshot (concurrent edit, symlink swap, metadata
/// change) aborts the whole batch with a [`MemoryError::Validation`] before any
/// file is written. Per-file writes are then applied sequentially; each is
/// individually atomic, but the batch is not. If a write fails after earlier
/// files were already written (e.g. the canonical `AGENTS.md` succeeds but a
/// dependent then fails), the returned error names both the file that failed
/// and the files already updated, so the caller can report the inconsistency
/// and re-run to reconcile.
pub fn apply_write_rules_plan(
    plan: &[WritePlanItem],
) -> Result<Vec<(PathBuf, WriteOutcome)>, MemoryError> {
    let mut preflight_matches: Vec<bool> = Vec::with_capacity(plan.len());
    for item in plan {
        let (metadata, existing) = read_rules_target(&item.target_path)?;
        if !existing_contents_and_metadata_match(item, &existing, &metadata) {
            return Err(MemoryError::Validation(format!(
                "ironmem write-rules preflight detected out-of-date state for {}",
                item.target_path.display()
            )));
        }
        let _ = upsert_block(&existing, &item.planned_contents)?;
        preflight_matches.push(existing == item.planned_contents);
    }

    let mut outcomes = Vec::new();
    let mut written: Vec<PathBuf> = Vec::new();
    for (item, unchanged) in plan.iter().zip(preflight_matches.iter()) {
        let outcome = if *unchanged {
            WriteOutcome::Unchanged
        } else {
            if let Err(error) = write_atomic(
                &item.target_path,
                &item.planned_contents,
                item.existing_metadata.as_ref(),
            ) {
                return Err(partial_write_error(&item.target_path, &written, error));
            }
            written.push(item.target_path.clone());
            if item.existing_metadata.is_some() {
                WriteOutcome::Updated
            } else {
                WriteOutcome::Created
            }
        };
        outcomes.push((item.target_path.clone(), outcome));
    }

    Ok(outcomes)
}

/// Build the error returned when a write fails mid-batch.
///
/// If nothing was written yet, the original error is propagated unchanged. Once
/// at least one file has been written, the error is upgraded to name both the
/// failed file and the already-updated files so the partial state is never
/// silently discarded.
fn partial_write_error(failed: &Path, written: &[PathBuf], error: MemoryError) -> MemoryError {
    if written.is_empty() {
        return error;
    }
    let already = written
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    MemoryError::Validation(format!(
        "ironmem write-rules failed writing {failed}: {error}. Already updated: {already}. \
         These files may now be inconsistent with {failed} — re-run `ironmem write-rules` to reconcile.",
        failed = failed.display(),
    ))
}

fn existing_contents_and_metadata_match(
    item: &WritePlanItem,
    current: &str,
    current_metadata: &Option<std::fs::Metadata>,
) -> bool {
    if current == item.planned_contents {
        return true;
    }

    if current != item.existing_contents {
        return false;
    }

    match (&item.existing_metadata, current_metadata) {
        (None, None) => true,
        (Some(before), Some(after)) => metadata_unchanged(before, after),
        (None, Some(_)) => false,
        (Some(_), None) => false,
    }
}

fn metadata_unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if before.file_type() != after.file_type() {
        return false;
    }
    if before.permissions().readonly() != after.permissions().readonly() {
        return false;
    }
    if before.len() != after.len() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if before.permissions().mode() != after.permissions().mode() {
            return false;
        }
    }
    true
}

/// Single-file managed write for an arbitrary `target_path`/`contents` pair.
///
/// Intentionally retained as a standalone primitive for callers that write one
/// file directly rather than through a resolved harness plan. It shares the same
/// [`upsert_block`] and [`write_atomic`] building blocks as
/// [`apply_write_rules_plan`], so the write mechanism does not diverge between
/// the two paths. Inserts or replaces the managed block and writes atomically,
/// skipping the write when the result is byte-identical to the existing file.
pub fn write_rules_file(target_path: &Path, contents: &str) -> Result<WriteOutcome, MemoryError> {
    let (target_metadata, current) = read_rules_target(target_path)?;
    let existed = target_metadata.is_some();

    let block = render_block(contents);
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

fn managed_block_region(contents: &str) -> Result<Option<(usize, usize, usize)>, MemoryError> {
    let begins = contents.matches(BEGIN_MARKER).count();
    let ends = contents.matches(END_MARKER).count();

    match (begins, ends) {
        (0, 0) => Ok(None),
        (1, 1) => {
            let begin_idx = contents
                .find(BEGIN_MARKER)
                .expect("counted exactly one BEGIN marker");
            let end_idx = contents
                .find(END_MARKER)
                .expect("counted exactly one END marker");
            if begin_idx > end_idx {
                return Err(MemoryError::Validation(
                    "ironmem managed block is malformed: END marker precedes BEGIN marker".into(),
                ));
            }
            let mut region_end = end_idx + END_MARKER.len();
            region_end = if contents[region_end..].starts_with("\r\n") {
                region_end + 2
            } else if contents[region_end..].starts_with('\n') {
                region_end + 1
            } else {
                region_end
            };
            Ok(Some((begin_idx, end_idx, region_end)))
        }
        _ => Err(MemoryError::Validation(format!(
            "ironmem managed block is malformed: found {begins} BEGIN and {ends} END markers (expected exactly one of each)"
        ))),
    }
}

fn flatten_and_rewrap_agent_canonical_rules(contents: &str) -> Result<String, MemoryError> {
    let (begin_idx, end_idx, end_region_end) =
        managed_block_region(contents)?.ok_or_else(|| {
            MemoryError::Validation(
                "ironmem managed block is malformed: no markers found in canonical rules".into(),
            )
        })?;

    let mut body = &contents[begin_idx + BEGIN_MARKER.len()..end_idx];
    body = strip_single_newline(body).ok_or_else(|| {
        MemoryError::Validation(
            "ironmem managed block is malformed: expected newline after BEGIN marker".into(),
        )
    })?;

    if !body.starts_with(NOTICE) {
        return Err(MemoryError::Validation(
            "ironmem managed block is malformed: missing managed NOTICE line".into(),
        ));
    }
    body = &body[NOTICE.len()..];
    let flattened = strip_single_newline(body).ok_or_else(|| {
        MemoryError::Validation(
            "ironmem managed block is malformed: expected managed contents line".into(),
        )
    })?;

    let before = &contents[..begin_idx];
    let after = &contents[end_region_end..];
    let flattened = format!("{before}{flattened}{after}");
    let flattened = strip_trailing_single_line_ending(flattened);
    Ok(flattened)
}

fn strip_single_newline(value: &str) -> Option<&str> {
    if let Some(next) = value.strip_prefix("\r\n") {
        return Some(next);
    }
    value.strip_prefix('\n')
}

fn strip_trailing_single_newline(value: &str) -> Option<&str> {
    if let Some(next) = value.strip_suffix("\r\n") {
        return Some(next);
    }
    value.strip_suffix('\n')
}

fn strip_trailing_single_line_ending(value: String) -> String {
    if let Some(stripped) = strip_trailing_single_newline(&value) {
        return stripped.to_owned();
    }
    value
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

fn temp_path_for(path: &Path) -> PathBuf {
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
    fn render_block_is_deterministic_and_contains_contents() {
        let a = render_block(PROTO);
        let b = render_block(PROTO);
        assert_eq!(a, b, "render must be deterministic");
        assert!(a.contains(PROTO), "block must contain the managed contents");
        assert!(a.starts_with(BEGIN_MARKER));
        assert!(a.trim_end().ends_with(END_MARKER));
        assert_eq!(a.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(a.matches(END_MARKER).count(), 1);
        assert!(a.ends_with('\n'), "block ends with a trailing newline");
    }

    #[test]
    fn render_block_wraps_arbitrary_contents() {
        let directive = "@./AGENTS.md";
        let a = render_block(directive);
        let b = render_block(directive);
        assert_eq!(a, b);
        assert!(a.contains(directive));
        assert_eq!(a.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(a.matches(END_MARKER).count(), 1);
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

    // ---- plan helpers ---------------------------------------------

    use crate::harness::{HarnessSpec, RulesStrategy, TranscriptParserKind, REGISTRY};

    const GEMINI_SPEC: HarnessSpec = HarnessSpec {
        id: "gemini",
        display_name: "Gemini CLI",
        binary: "gemini",
        rules_file: "GEMINI.md",
        rules_strategy: RulesStrategy::Import {
            directive: "@./AGENTS.md",
        },
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
    fn resolve_write_targets_target_can_dedupe_duplicate_native_rules_files() {
        const AGENTS_NATIVE_1: HarnessSpec = HarnessSpec {
            id: "agents-native-1",
            display_name: "Agents Native 1",
            binary: "agents-native-1",
            rules_file: "AGENTS.md",
            rules_strategy: RulesStrategy::Native,
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        };

        const AGENTS_NATIVE_2: HarnessSpec = HarnessSpec {
            id: "agents-native-2",
            display_name: "Agents Native 2",
            binary: "agents-native-2",
            rules_file: "AGENTS.md",
            rules_strategy: RulesStrategy::Native,
            write_rules_default: false,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        };

        let reg = [REGISTRY[0], REGISTRY[1], AGENTS_NATIVE_1, AGENTS_NATIVE_2];
        let targets = resolve_write_targets(Some("AGENTS.md"), None, &reg).unwrap();
        assert_eq!(targets, vec!["AGENTS.md"]);
        let defaults = resolve_write_targets(None, None, &reg).unwrap();
        assert_eq!(defaults, vec!["CLAUDE.md", "AGENTS.md"]);
    }

    #[test]
    fn resolve_write_targets_target_rejects_conflicting_rules_strategy() {
        const SPEC_IMPORT: HarnessSpec = HarnessSpec {
            id: "agents-import",
            display_name: "Agents Import",
            binary: "agents-import",
            rules_file: "CLAUDE.md",
            rules_strategy: RulesStrategy::Import {
                directive: "@AGENTS.md",
            },
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        };

        const SPEC_COPY: HarnessSpec = HarnessSpec {
            id: "agents-copy",
            display_name: "Agents Copy",
            binary: "agents-copy",
            rules_file: "CLAUDE.md",
            rules_strategy: RulesStrategy::Copy,
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        };

        const SPECS: [HarnessSpec; 4] = [REGISTRY[0], REGISTRY[1], SPEC_IMPORT, SPEC_COPY];

        let err = resolve_write_targets(Some("CLAUDE.md"), None, &SPECS).unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, MemoryError::Validation(_)));
        assert!(
            message.contains("conflicting rules_strategy"),
            "expected conflict error, got: {message}"
        );

        let err = resolve_write_targets(None, None, &SPECS).unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, MemoryError::Validation(_)));
        assert!(
            message.contains("conflicting rules_strategy"),
            "expected conflict error, got: {message}"
        );
    }

    #[test]
    fn default_rules_targets_excludes_non_default_synthetic_gemini() {
        let reg = three_entry_registry();
        let targets = crate::harness::default_rules_targets(&reg)
            .expect("default target resolution should succeed for non-conflicting registry");
        assert!(
            !targets.contains(&"GEMINI.md"),
            "non-default synthetic entry must be excluded"
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
        let targets = crate::harness::default_rules_targets(&reg)
            .expect("default target resolution should succeed for non-conflicting registry");
        assert!(!targets.contains(&"GEMINI.md"));
        assert_eq!(targets, vec!["CLAUDE.md", "AGENTS.md"]);
    }

    // ---- plan tests (new behavior) ----------------------------------

    const DECLARED_COPY_SPEC: HarnessSpec = HarnessSpec {
        id: "agents-copy-harness",
        display_name: "Agents Copy Harness",
        binary: "agents-copy-harness",
        rules_file: "CLAUDE.md",
        rules_strategy: RulesStrategy::Copy,
        write_rules_default: true,
        client_info_aliases: &[],
        env_aliases: &[],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    const DECLARED_IMPORT_SPEC: HarnessSpec = HarnessSpec {
        id: "agents-import-harness",
        display_name: "Agents Import Harness",
        binary: "agents-import-harness",
        rules_file: "CLAUDE.md",
        rules_strategy: RulesStrategy::Import {
            directive: "@./AGENTS.md",
        },
        write_rules_default: false,
        client_info_aliases: &[],
        env_aliases: &[],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    const SYNTHEX_GROK_SPEC: HarnessSpec = HarnessSpec {
        id: "grok",
        display_name: "Synthetic Grok",
        binary: "grok",
        rules_file: "AGENTS.md",
        rules_strategy: RulesStrategy::Native,
        write_rules_default: false,
        client_info_aliases: &[],
        env_aliases: &["grok"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    fn to_path_contents(plan: &[WritePlanItem]) -> Vec<(PathBuf, String)> {
        plan.iter()
            .map(|item| (item.target_path.clone(), item.planned_contents.clone()))
            .collect()
    }

    #[test]
    fn build_plan_for_import_uses_directive_without_protocol_duplicate() {
        let reg = [DECLARED_IMPORT_SPEC];
        let dir = tempfile::tempdir().unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();

        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].target_path.file_name().unwrap(),
            CANONICAL_RULES_FILE
        );
        assert_eq!(plan[1].target_path.file_name().unwrap(), "CLAUDE.md");
        assert_eq!(plan[1].planned_contents, render_block("@./AGENTS.md"));
        assert!(!plan[1]
            .planned_contents
            .contains(crate::bootstrap::MEMORY_PROTOCOL));
    }

    #[test]
    fn build_plan_for_copy_preserves_human_text_protocol_and_single_wrapped_markers() {
        let reg = [REGISTRY[1], DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CANONICAL_RULES_FILE),
            "# Human-authored block\n\nold\n\n",
        )
        .unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        assert_eq!(plan.len(), 2);

        let copy_entry = &plan[1];
        assert_eq!(copy_entry.target_path.file_name().unwrap(), "CLAUDE.md");
        assert_eq!(copy_entry.planned_contents.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(copy_entry.planned_contents.matches(END_MARKER).count(), 1);
        assert!(copy_entry
            .planned_contents
            .contains("# Human-authored block"));
        assert!(copy_entry
            .planned_contents
            .contains(crate::bootstrap::MEMORY_PROTOCOL));
        assert_eq!(
            copy_entry
                .planned_contents
                .matches(crate::bootstrap::MEMORY_PROTOCOL)
                .count(),
            1
        );
    }

    #[test]
    fn build_plan_for_copy_has_flattened_single_newline_before_end_marker() {
        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let copy_entry = &plan[1];
        assert_eq!(
            copy_entry.planned_contents,
            render_block(crate::bootstrap::MEMORY_PROTOCOL)
        );
        let no_double_newline = format!(
            "{NOTICE}\n{}\n{END_MARKER}\n",
            crate::bootstrap::MEMORY_PROTOCOL
        );
        assert!(
            copy_entry.planned_contents.ends_with(&no_double_newline),
            "flattened copy payload must terminate protocol with single newline"
        );
        let double_newline = format!(
            "{NOTICE}\n{}\n\n{END_MARKER}\n",
            crate::bootstrap::MEMORY_PROTOCOL
        );
        assert!(
            !copy_entry.planned_contents.contains(&double_newline),
            "exactly one newline is required between protocol and outer END marker"
        );
    }

    #[test]
    fn build_plan_for_copy_with_trailing_human_content_has_single_newline_before_end_marker() {
        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_input = format!(
            "# Human-authored block\n{BEGIN_MARKER}\n{NOTICE}\n{protocol}\n{END_MARKER}\nTrailing note\n",
            protocol = crate::bootstrap::MEMORY_PROTOCOL,
        );

        std::fs::write(dir.path().join(CANONICAL_RULES_FILE), canonical_input).unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let copy_entry = &plan[1];
        assert_eq!(copy_entry.target_path.file_name().unwrap(), "CLAUDE.md");

        let flattened_single = format!("Trailing note\n{END_MARKER}\n");
        let flattened_double = format!("Trailing note\n\n{END_MARKER}\n");
        assert!(
            copy_entry.planned_contents.contains(&flattened_single),
            "trailing human content must remain and be followed by exactly one newline"
        );
        assert!(
            !copy_entry.planned_contents.contains(&flattened_double),
            "flattening must normalize one trailing line ending before render_block"
        );
    }

    #[test]
    fn build_plan_for_copy_rejects_malformed_markers() {
        let reg = [REGISTRY[1], DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CANONICAL_RULES_FILE),
            format!(
                "# Human-authored block\n{}\n{}\n",
                BEGIN_MARKER, // reversed: missing end marker
                "No managed payload"
            ),
        )
        .unwrap();

        assert!(build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).is_err());
    }

    #[test]
    fn build_plan_for_copy_rejects_duplicate_markers() {
        let reg = [REGISTRY[1], DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let malformed = format!(
            "{BEGIN_MARKER}\n{NOTICE}\nfirst\n{END_MARKER}\n{BEGIN_MARKER}\nsecond\n{END_MARKER}\n"
        );
        std::fs::write(dir.path().join(CANONICAL_RULES_FILE), malformed).unwrap();

        assert!(build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).is_err());
    }

    #[test]
    fn build_plan_for_copy_rejects_reversed_markers() {
        let reg = [REGISTRY[1], DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let malformed = format!("{END_MARKER}\n{NOTICE}\nbody\n{BEGIN_MARKER}\n");
        std::fs::write(dir.path().join(CANONICAL_RULES_FILE), malformed).unwrap();

        assert!(build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).is_err());
    }

    #[test]
    fn build_plan_for_non_native_target_writes_canonical_before_dependent() {
        let reg = [DECLARED_IMPORT_SPEC];
        let dir = tempfile::tempdir().unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        assert!(plan.len() >= 2);
        assert_eq!(
            plan[0].target_path.file_name().unwrap(),
            CANONICAL_RULES_FILE
        );
        assert_eq!(plan[1].target_path.file_name().unwrap(), "CLAUDE.md");
    }

    #[test]
    fn build_plan_for_native_harness_collapse_with_synthetic_grok() {
        let reg = [REGISTRY[1], SYNTHEX_GROK_SPEC];
        let dir = tempfile::tempdir().unwrap();

        let plan = build_write_rules_plan(dir.path(), None, Some("grok"), &reg).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].target_path.file_name().unwrap(),
            CANONICAL_RULES_FILE
        );
    }

    #[test]
    fn build_plan_for_duplicate_invocation_is_byte_identical() {
        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();

        let first = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let second = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();

        assert_eq!(to_path_contents(&first), to_path_contents(&second));
    }

    #[test]
    fn apply_write_rules_plan_preserves_outcomes() {
        let reg = [DECLARED_IMPORT_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();

        let first = apply_write_rules_plan(&plan).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(
            first[0].0.file_name().unwrap(),
            CANONICAL_RULES_FILE,
            "canonical AGENTS.md must be the first outcome"
        );
        assert_eq!(first[1].0.file_name().unwrap(), "CLAUDE.md");
        assert!(
            first
                .iter()
                .all(|(_, outcome)| *outcome == WriteOutcome::Created),
            "both files must be created on first apply; got {first:?}"
        );

        let second = apply_write_rules_plan(&plan).unwrap();
        assert_eq!(second.len(), 2);
        assert!(
            second
                .iter()
                .all(|(_, outcome)| *outcome == WriteOutcome::Unchanged),
            "both files must be unchanged on re-apply; got {second:?}"
        );
    }

    #[test]
    fn apply_write_rules_plan_reapplies_existing_file_without_stale_metadata_failure() {
        let reg = [DECLARED_IMPORT_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");

        std::fs::write(&canonical_path, "# canonical before\n").unwrap();
        std::fs::write(&dependency_path, "# claude before\n").unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();

        let first = apply_write_rules_plan(&plan).unwrap();
        assert_eq!(first.len(), 2);
        assert!(first
            .iter()
            .all(|(_, outcome)| *outcome == WriteOutcome::Updated));

        let second = apply_write_rules_plan(&plan).unwrap();
        assert!(second
            .iter()
            .all(|(_, outcome)| *outcome == WriteOutcome::Unchanged));
    }

    #[cfg(unix)]
    #[test]
    fn apply_write_rules_plan_preflight_symlink_dependency_aborts_without_writes() {
        use std::os::unix::fs::symlink;

        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");
        let target_file = dir.path().join("REAL_CLAUDE.md");

        std::fs::write(&canonical_path, "# canonical before\n").unwrap();
        std::fs::write(&target_file, "# real dependent\n").unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let canonical_before = std::fs::read_to_string(&canonical_path).unwrap();

        std::fs::remove_file(&dependency_path).ok();
        symlink(&target_file, &dependency_path).unwrap();

        let err = apply_write_rules_plan(&plan).unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)));

        assert_eq!(
            std::fs::read_to_string(&canonical_path).unwrap(),
            canonical_before
        );
        assert!(std::fs::symlink_metadata(&dependency_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn apply_write_rules_plan_preflight_malformed_dependency_contents_aborts_without_writes() {
        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");

        std::fs::write(&canonical_path, "# canonical before\n").unwrap();
        std::fs::write(&dependency_path, "# stale dependent\n").unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let canonical_before = std::fs::read_to_string(&canonical_path).unwrap();

        let malformed = format!("{BEGIN_MARKER}\n{NOTICE}\nno trailing end\n");
        std::fs::write(&dependency_path, malformed).unwrap();

        let err = apply_write_rules_plan(&plan).unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)));
        assert_eq!(
            std::fs::read_to_string(&canonical_path).unwrap(),
            canonical_before
        );
    }

    #[test]
    fn apply_write_rules_plan_preflight_readability_error_aborts_without_writes() {
        let reg = [DECLARED_IMPORT_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");

        std::fs::write(&canonical_path, "# canonical before\n").unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let canonical_before = std::fs::read_to_string(&canonical_path).unwrap();

        std::fs::remove_file(&dependency_path).ok();
        std::fs::create_dir(&dependency_path).unwrap();

        let err = apply_write_rules_plan(&plan).unwrap_err();
        assert!(matches!(err, MemoryError::Io(_)));
        assert_eq!(
            std::fs::read_to_string(&canonical_path).unwrap(),
            canonical_before
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_write_rules_plan_preflight_metadata_change_aborts_without_writes() {
        use std::os::unix::fs::PermissionsExt;

        let reg = [DECLARED_IMPORT_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");

        std::fs::write(&canonical_path, "# canonical before\n").unwrap();
        std::fs::write(&dependency_path, "# stale dependent\n").unwrap();
        std::fs::set_permissions(&canonical_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&dependency_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let canonical_before = std::fs::read_to_string(&canonical_path).unwrap();

        std::fs::set_permissions(&canonical_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&dependency_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = apply_write_rules_plan(&plan).unwrap_err();
        assert!(matches!(err, MemoryError::Validation(_)));
        assert_eq!(
            std::fs::read_to_string(&canonical_path).unwrap(),
            canonical_before
        );
        let mode = std::fs::metadata(&canonical_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn apply_write_rules_plan_preserves_existing_unix_modes() {
        use std::os::unix::fs::PermissionsExt;

        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");

        std::fs::write(&canonical_path, "# old canonical\n").unwrap();
        std::fs::write(&dependency_path, "# old claude\n").unwrap();
        std::fs::set_permissions(&canonical_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&dependency_path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let outcomes = apply_write_rules_plan(&plan).unwrap();
        assert!(outcomes
            .iter()
            .any(|(_, outcome)| *outcome == WriteOutcome::Updated));

        let canonical_mode = std::fs::metadata(&canonical_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let dependency_mode = std::fs::metadata(&dependency_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(canonical_mode, 0o600);
        assert_eq!(dependency_mode, 0o640);
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

    #[test]
    fn apply_write_rules_plan_writes_copy_body_to_disk_and_is_idempotent() {
        let reg = [DECLARED_COPY_SPEC];
        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let dependency_path = dir.path().join("CLAUDE.md");

        let plan = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let expected_copy = plan[1].planned_contents.clone();

        let first = apply_write_rules_plan(&plan).unwrap();
        assert!(
            first
                .iter()
                .all(|(_, outcome)| *outcome == WriteOutcome::Created),
            "both files must be created on first apply; got {first:?}"
        );

        // The flattened Copy body must actually land on disk byte-for-byte — the
        // build tests only assert on planned_contents, never the written file.
        assert_eq!(
            std::fs::read_to_string(&dependency_path).unwrap(),
            expected_copy,
            "dependent CLAUDE.md must contain the flattened copy body on disk"
        );
        assert!(std::fs::read_to_string(&canonical_path)
            .unwrap()
            .contains(BEGIN_MARKER));

        // Re-deriving against the now-updated files and re-applying is a no-op.
        let plan2 = build_write_rules_plan(dir.path(), Some("CLAUDE.md"), None, &reg).unwrap();
        let before = std::fs::read(&dependency_path).unwrap();
        let second = apply_write_rules_plan(&plan2).unwrap();
        assert!(
            second
                .iter()
                .all(|(_, outcome)| *outcome == WriteOutcome::Unchanged),
            "re-apply must be unchanged on disk; got {second:?}"
        );
        assert_eq!(
            std::fs::read(&dependency_path).unwrap(),
            before,
            "copy body must be byte-stable across re-apply"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_write_rules_plan_reports_partial_write_when_dependent_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let canonical_path = dir.path().join(CANONICAL_RULES_FILE);
        let locked_dir = dir.path().join("locked");
        std::fs::create_dir(&locked_dir).unwrap();
        let dependency_path = locked_dir.join("CLAUDE.md");

        std::fs::write(&canonical_path, "# canonical before\n").unwrap();
        std::fs::write(&dependency_path, "# claude before\n").unwrap();

        // Hand-build a two-item plan whose targets live in different directories,
        // so only the dependent's write fails while the canonical write succeeds.
        let canonical_existing = std::fs::read_to_string(&canonical_path).unwrap();
        let dependency_existing = std::fs::read_to_string(&dependency_path).unwrap();
        let canonical_planned = upsert_block(
            &canonical_existing,
            &render_block(crate::bootstrap::MEMORY_PROTOCOL),
        )
        .unwrap();
        let dependency_planned =
            upsert_block(&dependency_existing, &render_block("@AGENTS.md")).unwrap();
        let plan = vec![
            WritePlanItem {
                target_path: canonical_path.clone(),
                planned_contents: canonical_planned.clone(),
                existing_metadata: std::fs::symlink_metadata(&canonical_path).ok(),
                existing_contents: canonical_existing,
            },
            WritePlanItem {
                target_path: dependency_path.clone(),
                planned_contents: dependency_planned,
                existing_metadata: std::fs::symlink_metadata(&dependency_path).ok(),
                existing_contents: dependency_existing,
            },
        ];

        // Make the dependent's directory unwritable so its atomic write fails
        // after the canonical write (in the still-writable parent) has landed.
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Skip where the process can write despite 0o500 (e.g. running as root),
        // where the permission-denied path this test exercises cannot occur.
        let probe = locked_dir.join(".ironmem-write-probe");
        if std::fs::File::create(&probe).is_ok() {
            std::fs::remove_file(&probe).ok();
            std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }

        let err = apply_write_rules_plan(&plan).unwrap_err();
        // Restore write permission so the tempdir can be cleaned up.
        std::fs::set_permissions(&locked_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(err, MemoryError::Validation(_)));
        let message = err.to_string();
        assert!(
            message.contains("CLAUDE.md"),
            "error must name the failed dependent file; got: {message}"
        );
        assert!(
            message.contains(CANONICAL_RULES_FILE),
            "error must name the already-updated canonical file; got: {message}"
        );
        assert!(
            message.contains("re-run"),
            "error must tell the user how to reconcile; got: {message}"
        );

        // The canonical file was written before the failure — the partial state is
        // real and surfaced, not silently hidden.
        assert_eq!(
            std::fs::read_to_string(&canonical_path).unwrap(),
            canonical_planned,
            "canonical file must reflect the completed write"
        );
        // The dependent file was never written.
        assert_eq!(
            std::fs::read_to_string(&dependency_path).unwrap(),
            "# claude before\n",
            "failed dependent file must be left untouched"
        );
    }
}
