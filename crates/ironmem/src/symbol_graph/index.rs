//! Incremental repo indexer for the symbol/import graph.
//!
//! Strategy (content-hash based, not diff-against-head):
//! - Walk all supported source files via `ignore::WalkBuilder`.
//! - For each file: compute `content_hash = sha256(bytes)`.
//!   - New file (no DB row) OR hash changed OR `--force` → reparse + replace.
//!   - Hash unchanged → skip.
//! - Files present in DB but absent from the walk → **purge** (handles deletes).
//! - Rename = delete(old path) purged + new path indexed.
//! - HEAD: resolved once per run; placeholder "0000..." used when no commits.
//!
//! All stored paths are normalized to repo-relative forward-slash.
//! Oversized (>1 MiB) and binary files are skipped with a warning.
//! The indexer returns `IndexResult` with per-category counts.

use std::collections::HashSet;
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use super::model::IndexResult;
use super::parse::{detect_language, parse_file};
use crate::db::schema::Database;
use crate::db::symbol_graph::{validate_repo_path, MAX_SNIPPET_LEN};
use crate::error::MemoryError;

/// Placeholder SHA used when the repo has no commits yet.
const NO_COMMIT_SHA: &str = "0000000000000000000000000000000000000000";

/// Maximum file size to index (1 MiB).
const MAX_FILE_BYTES: usize = 1 << 20;

// ── Public entry point ────────────────────────────────────────────────────────

/// Index all supported source files in `repo_path`.
///
/// `force` re-indexes even files whose content hash is unchanged.
///
/// # Errors
///
/// Returns `MemoryError` only for invalid `repo_path` (not a directory, not
/// a git worktree, or path contains traversal). File-level errors (unreadable
/// files, binary content) are accumulated as warnings in `IndexResult`.
pub fn index_repo(db: &Database, repo_path: &str, force: bool) -> Result<IndexResult, MemoryError> {
    let canonical = canonicalize_repo(repo_path)?;

    // Resolve HEAD once for the entire run.
    let (head_sha, head_resolved) = resolve_head(&canonical);

    let indexed_at = chrono::Utc::now().to_rfc3339();

    // Walk all supported source files.
    let walker = ignore::WalkBuilder::new(&canonical)
        .hidden(false)
        .git_ignore(true)
        .build();

    let mut files_indexed: usize = 0;
    let mut files_skipped: usize = 0;
    let mut symbols_inserted: usize = 0;
    let mut imports_inserted: usize = 0;
    let mut edges_inserted: usize = 0;

    // Track all rel-paths encountered in this walk for removed-file purge.
    let mut walked_paths: HashSet<String> = HashSet::new();

    for entry in walker.flatten() {
        let abs_path = entry.path().to_path_buf();
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }

        let canonical_file = match std::fs::canonicalize(&abs_path) {
            Ok(path) => path,
            Err(e) => {
                eprintln!(
                    "[symbol-graph] warn: could not canonicalize {}: {e}",
                    abs_path.display()
                );
                files_skipped += 1;
                continue;
            }
        };
        if !canonical_file.starts_with(Path::new(&canonical)) {
            eprintln!(
                "[symbol-graph] warn: skipping out-of-repo file: {}",
                abs_path.display()
            );
            files_skipped += 1;
            continue;
        }

        // Only process supported languages.
        if detect_language(&abs_path).is_empty() {
            continue;
        }

        let rel_path = match repo_relative_path(&canonical, &abs_path) {
            Some(p) => p,
            None => continue,
        };

        walked_paths.insert(rel_path.clone());

        // Read file bytes from the canonicalized, boundary-checked path (not
        // `abs_path`). Reading `canonical_file` closes the TOCTOU window: the
        // path we verified is inside the repo is the same one we read, so a
        // symlink swapped in after the `starts_with` check cannot redirect the
        // read outside the repo.
        let bytes = match std::fs::read(&canonical_file) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[symbol-graph] warn: could not read {}: {e}",
                    abs_path.display()
                );
                files_skipped += 1;
                continue;
            }
        };

        // Skip oversized files.
        if bytes.len() > MAX_FILE_BYTES {
            eprintln!(
                "[symbol-graph] warn: skipping {} ({} bytes > {MAX_FILE_BYTES})",
                rel_path,
                bytes.len()
            );
            files_skipped += 1;
            continue;
        }

        // Skip binary files (heuristic: NUL byte in first 8 KiB).
        if is_binary(&bytes) {
            eprintln!("[symbol-graph] warn: skipping binary file: {rel_path}");
            files_skipped += 1;
            continue;
        }

        // Compute content hash.
        let content_hash = sha256_hex(&bytes);

        // Incremental decision: skip if unchanged and not forced.
        if !force {
            if let Some(prior) = db.get_index_file(&canonical, &rel_path)? {
                if prior.content_hash == content_hash {
                    files_skipped += 1;
                    continue;
                }
            }
        }

        // Parse file content.
        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("[symbol-graph] warn: skipping non-UTF-8 file: {rel_path}");
                files_skipped += 1;
                continue;
            }
        };

        let parsed = parse_file(&abs_path, content);
        if !parsed.warnings.is_empty() {
            for w in &parsed.warnings {
                eprintln!("[symbol-graph] warn: {rel_path}: {w}");
            }
        }

        let language = &parsed.language;

        // Per-file replacement: delete old rows, insert new.
        db.with_transaction(|tx| {
            // Remove stale rows.
            Database::delete_file_rows_tx(tx, &canonical, &rel_path)?;

            // Upsert file-tracking row.
            Database::upsert_index_file_tx(
                tx,
                &canonical,
                &rel_path,
                &head_sha,
                &content_hash,
                language,
                &indexed_at,
            )?;

            let mut local_symbols = 0usize;
            let mut local_imports = 0usize;
            let mut local_edges = 0usize;

            // Build a map from qualified_name → id for parent resolution.
            let mut sym_id_map: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();

            // Insert symbols.
            for sym in &parsed.symbols {
                let id = symbol_id(
                    &canonical,
                    &rel_path,
                    &sym.kind,
                    &sym.qualified_name,
                    sym.start_line,
                    sym.start_col,
                );
                let parent_id = sym
                    .parent_qualified_name
                    .as_ref()
                    .and_then(|pqn| sym_id_map.get(pqn))
                    .cloned();

                // Count only rows actually persisted: `INSERT OR IGNORE` skips
                // id collisions, so an unconditional increment would overcount
                // and silently hide the dropped row.
                local_symbols += Database::insert_symbol_tx(
                    tx,
                    &id,
                    &canonical,
                    &rel_path,
                    language,
                    &sym.name,
                    &sym.qualified_name,
                    &sym.kind,
                    sym.visibility.as_deref(),
                    sym.signature.as_deref().map(truncate_snippet).as_deref(),
                    sym.start_line as i64,
                    sym.start_col as i64,
                    sym.end_line.map(|v| v as i64),
                    parent_id.as_deref(),
                    sym.confidence,
                    &indexed_at,
                )?;

                // Register `contains` edge if there is a parent.
                if let Some(ref pid) = parent_id {
                    let edge_id =
                        sha256_hex_str(&format!("{canonical}:{rel_path}:contains:{id}:{pid}"));
                    local_edges += Database::insert_edge_tx(
                        tx,
                        &edge_id,
                        &canonical,
                        "symbol",
                        &id,
                        "symbol",
                        pid,
                        "contains",
                        &rel_path,
                        Some(sym.start_line as i64),
                        1.0,
                        &indexed_at,
                    )?;
                }

                sym_id_map.insert(sym.qualified_name.clone(), id);
            }

            // Insert imports and `import` edges.
            for imp in &parsed.imports {
                let id = import_id(&canonical, &rel_path, &imp.module, imp.line);
                local_imports += Database::insert_import_tx(
                    tx,
                    &id,
                    &canonical,
                    &rel_path,
                    language,
                    &imp.module,
                    imp.symbol.as_deref(),
                    imp.alias.as_deref(),
                    imp.raw.as_deref().map(truncate_snippet).as_deref(),
                    imp.line as i64,
                    imp.confidence,
                    &indexed_at,
                )?;

                // `import` edge: file → module. The id includes the line so two
                // imports of the same module on distinct lines yield distinct
                // edges (matching `import_id`); without it the second edge
                // collides under INSERT OR IGNORE and is silently dropped while
                // `edges_inserted` still counts it.
                let edge_id = sha256_hex_str(&format!(
                    "{canonical}:{rel_path}:import:{}:{}",
                    imp.module, imp.line
                ));
                local_edges += Database::insert_edge_tx(
                    tx,
                    &edge_id,
                    &canonical,
                    "file",
                    &rel_path,
                    "module",
                    &imp.module,
                    "import",
                    &rel_path,
                    Some(imp.line as i64),
                    imp.confidence,
                    &indexed_at,
                )?;
            }

            symbols_inserted += local_symbols;
            imports_inserted += local_imports;
            edges_inserted += local_edges;
            Ok(())
        })?;

        files_indexed += 1;
    }

    // ── Removed-file purge ────────────────────────────────────────────────
    // Any file in DB that wasn't walked must have been deleted or moved.
    let prior_files = db.list_index_files(&canonical)?;
    let mut files_purged: usize = 0;
    for prior in prior_files {
        if !walked_paths.contains(&prior.path) {
            db.with_transaction(|tx| Database::purge_file_tx(tx, &canonical, &prior.path))?;
            files_purged += 1;
        }
    }

    Ok(IndexResult {
        files_indexed,
        files_skipped,
        files_purged,
        symbols_inserted,
        imports_inserted,
        edges_inserted,
        head_resolved,
        head_sha,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Canonicalize and validate the repo path. Returns the canonical absolute path
/// as a String. Rejects non-directories, non-git-worktrees, and path traversal.
pub fn canonicalize_repo(raw: &str) -> Result<String, MemoryError> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(MemoryError::Validation("repo is required".into()));
    }

    let raw_path = Path::new(trimmed);
    for component in raw_path.components() {
        if component == Component::ParentDir {
            return Err(MemoryError::Validation(format!(
                "repo must not contain parent-directory traversal: {trimmed}"
            )));
        }
    }

    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(raw_path)
    };

    let canonical = std::fs::canonicalize(&candidate).map_err(|e| {
        MemoryError::Validation(format!(
            "repo must be an existing directory: {trimmed}: {e}"
        ))
    })?;
    if !canonical.is_dir() {
        return Err(MemoryError::Validation(format!(
            "repo must be a directory: {}",
            canonical.display()
        )));
    }

    // Verify it is a git worktree.
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&canonical)
        .output()
        .map_err(|e| MemoryError::Validation(format!("git worktree check failed: {e}")))?;
    if !output.status.success() {
        return Err(MemoryError::Validation(format!(
            "repo must be a git worktree: {}",
            canonical.display()
        )));
    }

    Ok(canonical.to_string_lossy().to_string())
}

/// Resolve the current HEAD SHA. Returns (sha, true) on success, or the
/// placeholder "0000...0" + false when the repo has no commits.
fn resolve_head(repo: &str) -> (String, bool) {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.is_empty() {
                (NO_COMMIT_SHA.to_string(), false)
            } else {
                (sha, true)
            }
        }
        _ => (NO_COMMIT_SHA.to_string(), false),
    }
}

/// Convert an absolute path to a repo-relative forward-slash path.
/// Returns `None` if the path is not under the repo root.
fn repo_relative_path(repo: &str, abs: &Path) -> Option<String> {
    let repo_path = Path::new(repo);
    let rel = abs.strip_prefix(repo_path).ok()?;
    // Normalize to forward-slash on all platforms.
    let mut parts: Vec<String> = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(s) => parts.push(s.to_string_lossy().to_string()),
            _ => return None, // reject traversal
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Validate that `path` stays within `repo` (no traversal).
/// Returns `MemoryError::Validation` if `path` resolves outside the repo root.
pub fn validate_path_within_repo(repo: &str, path: &str) -> Result<(), MemoryError> {
    validate_repo_path(repo, path)?;
    // Path must not contain traversal components.
    for component in Path::new(path).components() {
        if component == Component::ParentDir {
            return Err(MemoryError::Validation(format!(
                "path must not traverse outside the repo: {path}"
            )));
        }
        if component == Component::RootDir || matches!(component, Component::Prefix(_)) {
            return Err(MemoryError::Validation(format!(
                "path must be repo-relative, not absolute: {path}"
            )));
        }
    }
    Ok(())
}

/// SHA-256 hex digest of bytes (for content hashing).
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// SHA-256 hex digest of a string (for deterministic IDs).
fn sha256_hex_str(s: &str) -> String {
    sha256_hex(s.as_bytes())
}

/// Deterministic symbol ID: sha256 of "repo:path:kind:qualified_name:line:col".
fn symbol_id(
    repo: &str,
    path: &str,
    kind: &str,
    qualified_name: &str,
    line: u32,
    col: u32,
) -> String {
    sha256_hex_str(&format!(
        "{repo}:{path}:{kind}:{qualified_name}:{line}:{col}"
    ))
}

/// Deterministic import ID: sha256 of "repo:path:import:module:line".
fn import_id(repo: &str, path: &str, module: &str, line: u32) -> String {
    sha256_hex_str(&format!("{repo}:{path}:import:{module}:{line}"))
}

/// Truncate a string snippet to `MAX_SNIPPET_LEN` bytes at a UTF-8 boundary.
fn truncate_snippet(s: &str) -> String {
    if s.len() <= MAX_SNIPPET_LEN {
        s.to_string()
    } else {
        let mut end = MAX_SNIPPET_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

/// Heuristic binary detection: check first 8 KiB for NUL bytes.
fn is_binary(bytes: &[u8]) -> bool {
    let check_len = bytes.len().min(8192);
    bytes[..check_len].contains(&0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;
    use std::path::PathBuf;
    use std::process::Command;

    /// Create a minimal git repo with initial commit; return (TempDir, root_path).
    fn make_git_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(&root)
            .output()
            .unwrap();
        (dir, root)
    }

    fn commit_all(root: &PathBuf) {
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "commit"])
            .current_dir(root)
            .output()
            .unwrap();
    }

    fn write_file(root: &Path, rel: &str, content: &str) {
        let full = root.join(rel);
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    // ── Task 4 acceptance: first-index counts ──────────────────────────────

    #[test]
    fn first_index_counts_files_and_symbols() {
        let (dir, root) = make_git_repo();
        write_file(&root, "src/lib.rs", "pub fn hello() {}\n");
        write_file(&root, "src/main.py", "def greet():\n    pass\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let result = index_repo(&db, &root.to_string_lossy(), false).unwrap();

        assert!(result.files_indexed >= 2, "should index at least 2 files");
        assert!(
            result.symbols_inserted >= 2,
            "should insert at least 2 symbols"
        );
        assert!(result.head_resolved, "head should be resolved after commit");

        drop(dir);
    }

    // ── Task 4 acceptance: unchanged file skipped on re-index ─────────────

    #[test]
    fn unchanged_file_skipped_on_reindex() {
        let (dir, root) = make_git_repo();
        write_file(&root, "lib.rs", "pub fn foo() {}\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let r1 = index_repo(&db, &root.to_string_lossy(), false).unwrap();
        assert_eq!(r1.files_indexed, 1);
        assert_eq!(r1.files_skipped, 0);

        // Re-index without changes → skipped.
        let r2 = index_repo(&db, &root.to_string_lossy(), false).unwrap();
        assert_eq!(r2.files_indexed, 0, "unchanged file must be skipped");
        assert_eq!(r2.files_skipped, 1, "unchanged file must appear in skipped");

        drop(dir);
    }

    // ── Task 4 acceptance: edited file reindexed ──────────────────────────

    #[test]
    fn edited_file_reindexed() {
        let (dir, root) = make_git_repo();
        write_file(&root, "lib.rs", "pub fn old() {}\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        index_repo(&db, &root_str, false).unwrap();
        // Use the canonical path (may differ from root_str on macOS with /private/tmp).
        let canonical = canonicalize_repo(&root_str).unwrap();

        // Verify old symbol exists.
        let syms_before = db.lookup_symbols(&canonical, "old", None, 10).unwrap();
        assert_eq!(syms_before.len(), 1);

        // Edit the file.
        write_file(&root, "lib.rs", "pub fn new_fn() {}\n");
        let r2 = index_repo(&db, &root_str, false).unwrap();
        assert_eq!(r2.files_indexed, 1, "edited file must be reindexed");

        // Old symbol gone, new symbol present.
        let syms_old = db.lookup_symbols(&canonical, "old", None, 10).unwrap();
        assert_eq!(syms_old.len(), 0, "old symbol must be replaced");

        let syms_new = db.lookup_symbols(&canonical, "new_fn", None, 10).unwrap();
        assert_eq!(syms_new.len(), 1, "new symbol must be present");

        drop(dir);
    }

    // ── Task 4 acceptance: deleted file purged ─────────────────────────────

    #[test]
    fn deleted_file_purged() {
        let (dir, root) = make_git_repo();
        write_file(&root, "lib.rs", "pub fn to_delete() {}\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        let syms = db
            .lookup_symbols(&canonical, "to_delete", None, 10)
            .unwrap();
        assert_eq!(syms.len(), 1, "symbol should exist after first index");

        // Delete the file and re-index.
        std::fs::remove_file(root.join("lib.rs")).unwrap();
        let r2 = index_repo(&db, &root_str, false).unwrap();
        assert_eq!(r2.files_purged, 1, "deleted file must be purged");

        let syms_after = db
            .lookup_symbols(&canonical, "to_delete", None, 10)
            .unwrap();
        assert_eq!(syms_after.len(), 0, "symbol must be purged");

        drop(dir);
    }

    // ── Task 4 acceptance: renamed file = old purged + new indexed ─────────

    #[test]
    fn renamed_file_old_purged_new_indexed() {
        let (dir, root) = make_git_repo();
        write_file(&root, "old_name.rs", "pub fn alpha() {}\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        // Rename: remove old, create new.
        std::fs::remove_file(root.join("old_name.rs")).unwrap();
        write_file(&root, "new_name.rs", "pub fn beta() {}\n");

        let r2 = index_repo(&db, &root_str, false).unwrap();
        assert_eq!(r2.files_purged, 1, "old file must be purged");
        assert_eq!(r2.files_indexed, 1, "new file must be indexed");

        let old_syms = db.lookup_symbols(&canonical, "alpha", None, 10).unwrap();
        assert_eq!(old_syms.len(), 0, "old symbol must be gone");

        let new_syms = db.lookup_symbols(&canonical, "beta", None, 10).unwrap();
        assert_eq!(new_syms.len(), 1, "new symbol must be present");

        drop(dir);
    }

    // ── Regression: same-module imports on distinct lines must not collide ──
    // Two `use std::io::*` statements on different lines yield two import rows
    // and must yield two distinct `import` edges; `edges_inserted` must equal
    // the number of edges actually persisted (no INSERT-OR-IGNORE drop +
    // unconditional overcount).
    #[test]
    fn same_module_imports_on_distinct_lines_produce_distinct_edges() {
        let (dir, root) = make_git_repo();
        write_file(&root, "lib.rs", "use std::io::Read;\nuse std::io::Write;\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        let result = index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        assert_eq!(
            result.imports_inserted, 2,
            "two same-module imports must yield two import rows"
        );

        let import_edges: Vec<_> = db
            .lookup_neighbors(&canonical, "std::io", 10)
            .unwrap()
            .into_iter()
            .filter(|edge| edge.edge_kind == "import")
            .collect();
        assert_eq!(
            import_edges.len(),
            2,
            "two same-module imports on distinct lines must persist two import edges"
        );
        assert_eq!(
            result.edges_inserted, 2,
            "edges_inserted must equal the number of edges actually persisted"
        );

        drop(dir);
    }

    #[test]
    fn neighbors_resolve_qualified_name_and_contains_points_to_parent() {
        let (dir, root) = make_git_repo();
        write_file(
            &root,
            "lib.rs",
            "pub mod outer {\n    pub fn inner() {}\n}\n",
        );
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        let child = db
            .lookup_symbols(&canonical, "outer::inner", None, 10)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.qualified_name == "outer::inner")
            .expect("child symbol should be indexed");
        let parent = db
            .lookup_symbols(&canonical, "outer", Some("mod"), 10)
            .unwrap()
            .into_iter()
            .find(|symbol| symbol.qualified_name == "outer")
            .expect("parent symbol should be indexed");
        let edges = db.lookup_neighbors(&canonical, "outer::inner", 10).unwrap();
        let contains = edges
            .iter()
            .find(|edge| edge.edge_kind == "contains")
            .expect("qualified-name neighbor lookup should return contains edge");

        assert_eq!(contains.from_id, child.id);
        assert_eq!(contains.to_ref, parent.id);

        drop(dir);
    }

    // ── Task 4 acceptance: no-commit repo → head_resolved=false, no error ──

    #[test]
    fn no_commit_repo_head_resolved_false_no_error() {
        let (dir, root) = make_git_repo();
        // Do NOT commit anything.
        write_file(&root, "lib.rs", "pub fn empty() {}\n");

        let db = Database::open_in_memory().unwrap();
        let result = index_repo(&db, &root.to_string_lossy(), false);
        // Must succeed (no error).
        let result = result.expect("index_repo must not error on no-commit repo");
        assert!(
            !result.head_resolved,
            "head_resolved must be false when repo has no commits"
        );
        assert_eq!(
            result.head_sha, NO_COMMIT_SHA,
            "head_sha must be the placeholder"
        );

        drop(dir);
    }

    #[cfg(unix)]
    #[test]
    fn out_of_repo_symlink_is_not_indexed() {
        use std::os::unix::fs::symlink;

        let (dir, root) = make_git_repo();
        let external = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(external.path(), "pub fn leaked_external() {}\n").unwrap();
        symlink(external.path(), root.join("leak.rs")).unwrap();
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        let result = index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();
        let leaked = db
            .lookup_symbols(&canonical, "leaked_external", None, 10)
            .unwrap();

        assert_eq!(result.files_indexed, 0, "symlink target must not be read");
        assert!(
            leaked.is_empty(),
            "out-of-repo symlink content must not be indexed"
        );

        drop(dir);
    }

    // ── validate_path_within_repo ──────────────────────────────────────────

    #[test]
    fn validate_path_within_repo_rejects_traversal() {
        assert!(validate_path_within_repo("/repo", "../outside.rs").is_err());
        assert!(validate_path_within_repo("/repo", "/absolute/path.rs").is_err());
        assert!(validate_path_within_repo("/repo", "src/lib.rs").is_ok());
    }

    // ── Defensive file-content skip paths ──────────────────────────────────

    #[test]
    fn oversized_file_is_skipped_not_indexed() {
        let (dir, root) = make_git_repo();
        // A valid symbol declaration followed by padding that pushes the file
        // past MAX_FILE_BYTES (1 MiB). The symbol must NOT be indexed.
        let padding = "// pad\n".repeat((MAX_FILE_BYTES / 7) + 1);
        let content = format!("pub fn oversized_symbol() {{}}\n{padding}");
        assert!(content.len() > MAX_FILE_BYTES, "fixture must exceed cap");
        write_file(&root, "big.rs", &content);
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        let result = index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        assert_eq!(
            result.files_indexed, 0,
            "oversized file must not be indexed"
        );
        assert_eq!(
            result.files_skipped, 1,
            "oversized file must be counted skipped"
        );
        let syms = db
            .lookup_symbols(&canonical, "oversized_symbol", None, 10)
            .unwrap();
        assert!(
            syms.is_empty(),
            "symbol from oversized file must not persist"
        );

        drop(dir);
    }

    #[test]
    fn binary_file_is_skipped_not_indexed() {
        let (dir, root) = make_git_repo();
        // Valid-looking source text with an embedded NUL byte in the first
        // 8 KiB → treated as binary and skipped.
        let mut bytes = b"pub fn binary_symbol() {}\n".to_vec();
        bytes.push(0);
        std::fs::write(root.join("bin.rs"), &bytes).unwrap();
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        let result = index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        assert_eq!(result.files_indexed, 0, "binary file must not be indexed");
        assert_eq!(
            result.files_skipped, 1,
            "binary file must be counted skipped"
        );
        let syms = db
            .lookup_symbols(&canonical, "binary_symbol", None, 10)
            .unwrap();
        assert!(syms.is_empty(), "symbol from binary file must not persist");

        drop(dir);
    }

    #[test]
    fn non_utf8_file_is_skipped_not_indexed() {
        let (dir, root) = make_git_repo();
        // Valid source prefix followed by an invalid UTF-8 byte (0xFF), with no
        // NUL so it passes the binary heuristic but fails UTF-8 decoding.
        let mut bytes = b"pub fn utf8_symbol() {}\n".to_vec();
        bytes.push(0xFF);
        std::fs::write(root.join("bad.rs"), &bytes).unwrap();
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        let result = index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        assert_eq!(
            result.files_indexed, 0,
            "non-UTF-8 file must not be indexed"
        );
        assert_eq!(
            result.files_skipped, 1,
            "non-UTF-8 file must be counted skipped"
        );
        let syms = db
            .lookup_symbols(&canonical, "utf8_symbol", None, 10)
            .unwrap();
        assert!(
            syms.is_empty(),
            "symbol from non-UTF-8 file must not persist"
        );

        drop(dir);
    }

    #[test]
    fn gitignored_file_is_not_indexed() {
        let (dir, root) = make_git_repo();
        write_file(&root, "kept.rs", "pub fn kept_symbol() {}\n");
        write_file(&root, "ignored.rs", "pub fn ignored_symbol() {}\n");
        write_file(&root, ".gitignore", "ignored.rs\n");
        commit_all(&root);

        let db = Database::open_in_memory().unwrap();
        let root_str = root.to_string_lossy().to_string();
        index_repo(&db, &root_str, false).unwrap();
        let canonical = canonicalize_repo(&root_str).unwrap();

        let kept = db
            .lookup_symbols(&canonical, "kept_symbol", None, 10)
            .unwrap();
        assert_eq!(kept.len(), 1, "non-ignored file must be indexed");
        let ignored = db
            .lookup_symbols(&canonical, "ignored_symbol", None, 10)
            .unwrap();
        assert!(ignored.is_empty(), "gitignored file must not be indexed");

        drop(dir);
    }
}
