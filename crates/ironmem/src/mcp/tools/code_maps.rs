//! MCP tool handlers for lazy per-area code maps (issue #94).
//! Tools: `code_map_write`, `code_map_load`, `code_map_status`.

use serde_json::{json, Value};
use std::path::{Component, Path};

use crate::code_maps::{classify, Freshness};
use crate::db::drawers::generate_id;
use crate::db::schema::Database;
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::sanitize;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn freshness_to_json(f: Freshness) -> Value {
    match f {
        Freshness::Fresh => json!({ "verdict": "fresh" }),
        Freshness::Stale { changed_files } => {
            json!({ "verdict": "stale", "changed_files": changed_files })
        }
        Freshness::RescoutRequired { reason } => {
            json!({ "verdict": "rescout_required", "reason": reason })
        }
    }
}

fn validate_source_files(source_files: &[&str]) -> Result<Vec<String>, MemoryError> {
    let mut out = Vec::with_capacity(source_files.len());
    for f in source_files {
        if f.is_empty() {
            return Err(MemoryError::Validation(
                "source_file must not be empty".into(),
            ));
        }
        if f.contains('\0') || f.contains('\\') {
            return Err(MemoryError::Validation(format!(
                "source_file must be a normalized repo-relative path: {f}"
            )));
        }

        let path = Path::new(f);
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    parts.push(part.to_string_lossy().to_string());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(MemoryError::Validation(format!(
                        "source_file must not traverse parent directories: {f}"
                    )));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(MemoryError::Validation(format!(
                        "source_file must be repo-relative, not absolute: {f}"
                    )));
                }
            }
        }
        if parts.is_empty() {
            return Err(MemoryError::Validation(format!(
                "source_file must name a repo-relative file: {f}"
            )));
        }
        out.push(parts.join("/"));
    }
    // Share the storage-layer byte invariant (non-empty set, no backslash/NUL,
    // no empty entry) so MCP and DB layers enforce one identical check.
    crate::db::code_maps::validate_source_files_storage(&out)?;
    Ok(out)
}

fn validate_repo(raw: &str) -> Result<String, MemoryError> {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(MemoryError::Validation("repo is required".into()));
    }

    let raw_path = Path::new(trimmed);
    if !raw_path.is_absolute() {
        return Err(MemoryError::Validation(format!(
            "repo must be an absolute path: {trimmed}"
        )));
    }
    for component in raw_path.components() {
        if component == Component::ParentDir {
            return Err(MemoryError::Validation(format!(
                "repo must not contain parent-directory traversal: {trimmed}"
            )));
        }
    }

    let canonical = std::fs::canonicalize(raw_path).map_err(|e| {
        MemoryError::Validation(format!(
            "repo must be an existing git worktree: {trimmed}: {e}"
        ))
    })?;
    if !canonical.is_dir() {
        return Err(MemoryError::Validation(format!(
            "repo must be a directory: {}",
            canonical.display()
        )));
    }

    // Scrubbed for the reason `scrub_git_environment` documents: an ambient
    // `GIT_DIR`/`GIT_WORK_TREE` overrides `current_dir`, so without this the
    // probe answers about whatever repo the environment names rather than the
    // one the caller asked about.
    let mut command = std::process::Command::new("git");
    crate::mcp::tools::scrub_git_environment(&mut command);
    let output = command
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
    let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let top = std::fs::canonicalize(&top).map_err(|e| {
        MemoryError::Validation(format!(
            "git worktree root could not be resolved: {top}: {e}"
        ))
    })?;
    Ok(top.to_string_lossy().to_string())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `code_map_write`'s arguments after validation. `summary` and `head_sha`
/// borrow from the request; the rest are owned/canonicalized.
pub(super) struct CodeMapWriteArgs<'a> {
    repo: String,
    area: String,
    summary: &'a str,
    head_sha: &'a str,
    source_files: Vec<String>,
    built_by: &'a str,
}

/// `code_map_write`'s arguments after everything that can be checked without
/// running `git`. `repo_raw` is only known to be *present* here; resolving it
/// to a worktree root is [`validate_repo`]'s job.
struct CodeMapWriteFields<'a> {
    repo_raw: &'a str,
    area: String,
    summary: &'a str,
    head_sha: &'a str,
    source_files: Vec<String>,
    built_by: &'a str,
}

/// The structural half of `code_map_write` validation: every field check that
/// does not shell out to `git`.
///
/// Split from [`validate_code_map_write_args`] so the daemon's pre-wait
/// precheck can reject a malformed call — the thing it exists to do — without
/// forking `git` on the thread that owns the `App`. The handler runs the full
/// validation after the gate resolves regardless, so the `git` check is not
/// skipped, only performed once instead of twice.
fn validate_code_map_write_fields(args: &Value) -> Result<CodeMapWriteFields<'_>, MemoryError> {
    let repo_raw = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("repo is required".into()))?;

    let area_raw = args
        .get("area")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("area is required".into()))?;
    let area = sanitize::sanitize_name(area_raw, "area")?;

    let summary_raw = args
        .get("summary")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("summary is required".into()))?;
    let summary = sanitize::sanitize_content(summary_raw, 100_000)?;

    let head_sha = args
        .get("head_sha")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MemoryError::Validation("head_sha is required".into()))?;
    // `head_sha` is the git object name the map was built at. Reject a
    // malformed (non-hex / wrong-length) value at write time using the same
    // shape check the freshness engine applies — otherwise a bad SHA persists a
    // map that is permanently `rescout_required`, wasting a scout each load.
    if !crate::code_maps::is_hex_sha(head_sha) {
        return Err(MemoryError::Validation(format!(
            "head_sha must be a hex git object name (7-64 hex chars): {head_sha}"
        )));
    }

    let source_files_raw = args
        .get("source_files")
        .and_then(|v| v.as_array())
        .ok_or_else(|| MemoryError::Validation("source_files must be an array".into()))?;
    if source_files_raw.is_empty() {
        return Err(MemoryError::Validation(
            "source_files must include at least one file".into(),
        ));
    }
    let source_files_raw: Vec<&str> = source_files_raw
        .iter()
        .map(|v| {
            v.as_str().ok_or_else(|| {
                MemoryError::Validation("source_files must contain only strings".into())
            })
        })
        .collect::<Result<_, _>>()?;
    let source_files = validate_source_files(&source_files_raw)?;

    let built_by = args
        .get("built_by")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MemoryError::Validation("built_by is required".into()))?;

    Ok(CodeMapWriteFields {
        repo_raw,
        area,
        summary,
        head_sha,
        source_files,
        built_by,
    })
}

/// Pre-wait precheck for `code_map_write`: structural validation only, no
/// `git` fork. See [`validate_code_map_write_fields`].
pub(super) fn precheck_code_map_write_args(args: &Value) -> Result<(), MemoryError> {
    validate_code_map_write_fields(args).map(drop)
}

/// Readiness-independent validation for `code_map_write` — see
/// `drawers::validate_add_drawer_args` for why this is split out. The full
/// check, including resolving `repo` to a git worktree root.
pub(super) fn validate_code_map_write_args(
    args: &Value,
) -> Result<CodeMapWriteArgs<'_>, MemoryError> {
    let fields = validate_code_map_write_fields(args)?;
    Ok(CodeMapWriteArgs {
        repo: validate_repo(fields.repo_raw)?,
        area: fields.area,
        summary: fields.summary,
        head_sha: fields.head_sha,
        source_files: fields.source_files,
        built_by: fields.built_by,
    })
}

pub(super) fn handle_code_map_write(app: &App, args: &Value) -> Result<Value, MemoryError> {
    // Readiness is already resolved by the time this runs (see
    // `tools::WRITE_SHAPED_TOOLS`); validation is split out so
    // `precheck_write_request` can reject a malformed call BEFORE the wait.
    let CodeMapWriteArgs {
        repo,
        area,
        summary,
        head_sha,
        source_files,
        built_by,
    } = validate_code_map_write_args(args)?;

    // turn_id is optional — just read it for wal/metrics if provided
    let _turn_id = args.get("turn_id").and_then(|v| v.as_str());

    // --- Embed ---
    app.ensure_embedder_ready()?;
    let embedding = {
        let mut emb = app
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        emb.embed_one(summary).map_err(MemoryError::Embed)?
    };

    let drawer_id = generate_id(summary, &repo, &format!("code-maps:{area}"));
    let built_at = chrono::Utc::now().to_rfc3339();

    // --- Atomic transaction: insert drawer + upsert sidecar ---
    let superseded = app.db.with_transaction(|tx| {
        Database::insert_drawer_tx(
            tx,
            &drawer_id,
            summary,
            &embedding,
            &repo,
            "code-maps",
            "",
            "mcp",
        )?;
        let superseded = Database::upsert_code_map_tx(
            tx,
            &repo,
            &area,
            &drawer_id,
            head_sha,
            &source_files,
            built_by,
            &built_at,
        )?;
        Ok(superseded)
    })?;

    // Update live HNSW index with the new drawer's vector.
    app.insert_into_index(&drawer_id, &embedding)?;

    // A refresh that superseded a prior drawer deleted that drawer in-tx; its
    // stale vector is still live in the in-memory HNSW index. Mark the index
    // dirty so the deleted vector is dropped on the next rebuild — mirroring
    // `handle_delete_drawer`.
    if superseded {
        app.mark_dirty();
    }

    Ok(json!({
        "success": true,
        "drawer_id": drawer_id,
        "wing": repo,
        "room": "code-maps",
    }))
}

pub(super) fn handle_code_map_load(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_raw = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("repo is required".into()))?;
    let repo = validate_repo(repo_raw)?;

    let area_raw = args
        .get("area")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("area is required".into()))?;
    let area = sanitize::sanitize_name(area_raw, "area")?;

    match app.db.get_code_map(&repo, &area)? {
        None => Ok(json!({
            "found": false,
            "freshness": freshness_to_json(Freshness::RescoutRequired {
                reason: "no map found".to_string(),
            }),
        })),
        Some(map) => {
            // Load drawer content. A missing drawer (Ok(None)) is an integrity
            // violation, NOT an empty summary: the sidecar row references a
            // drawer that no longer exists. Returning found:true + "" would let
            // a worker treat a hollow map as usable. Treat it as a hard miss →
            // found:false + rescout_required.
            let Some(drawer) = app.db.get_drawer(&map.drawer_id)? else {
                tracing::warn!(
                    repo = %map.repo,
                    area = %map.area,
                    drawer_id = %map.drawer_id,
                    "code_map_load: sidecar references a missing drawer; treating as rescout_required"
                );
                return Ok(json!({
                    "found": false,
                    "freshness": freshness_to_json(Freshness::RescoutRequired {
                        reason: "map content missing; re-scout required".to_string(),
                    }),
                }));
            };
            let summary = drawer.content;

            let freshness = classify(&map, std::path::Path::new(&map.repo));
            let freshness_json = freshness_to_json(freshness);

            Ok(json!({
                "found": true,
                "summary": summary,
                "meta": {
                    "repo": map.repo,
                    "area": map.area,
                    "drawer_id": map.drawer_id,
                    "head_sha": map.head_sha,
                    "source_files": map.source_files,
                    "built_by": map.built_by,
                    "built_at": map.built_at,
                },
                "freshness": freshness_json,
            }))
        }
    }
}

pub(super) fn handle_code_map_status(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_raw = args
        .get("repo")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("repo is required".into()))?;
    let repo = validate_repo(repo_raw)?;

    let area_raw = args
        .get("area")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MemoryError::Validation("area is required".into()))?;
    let area = sanitize::sanitize_name(area_raw, "area")?;

    match app.db.get_code_map(&repo, &area)? {
        None => Ok(json!({
            "found": false,
            "freshness": freshness_to_json(Freshness::RescoutRequired {
                reason: "no map found".to_string(),
            }),
        })),
        Some(map) => {
            let freshness = classify(&map, std::path::Path::new(&map.repo));
            let freshness_json = freshness_to_json(freshness);
            Ok(json!({
                "found": true,
                "freshness": freshness_json,
            }))
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpAccessMode;
    use crate::mcp::tools::call_tool;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_app() -> Arc<App> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(App::open_for_test().unwrap())
    }

    fn test_app_readonly() -> Arc<App> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(App::open_for_test_with_mode(McpAccessMode::ReadOnly).unwrap())
    }

    /// Run one git command in `root` with the environment scrubbed.
    ///
    /// The scrub is not decoration. `collab_session`'s test fixtures set
    /// `GIT_DIR`/`GIT_WORK_TREE` process-wide (see `ScopedGitEnv`), `cargo
    /// test` runs this binary's tests on parallel threads, and those variables
    /// override `current_dir` — so an unscrubbed `git init`/`add`/`commit`
    /// here intermittently built its repository inside the *other* test's
    /// fixture, leaving this one with no commits and failing it at random.
    /// The mutex `ScopedGitEnv` holds serializes the tests that set the
    /// variables, not the unrelated ones that merely inherit them.
    ///
    /// `commit.gpgsign=false` and `core.hooksPath=/dev/null` are pinned for a
    /// second, unrelated reason: a developer whose global git config signs
    /// commits or installs hooks would otherwise see these tests fail on a
    /// clean checkout.
    fn git_in(root: &std::path::Path, args: &[&str]) -> std::process::Output {
        let mut command = std::process::Command::new("git");
        crate::mcp::tools::scrub_git_environment(&mut command);
        command
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
    }

    // Set up a real git repo with a committed file and return (TempDir, root, sha).
    fn make_git_repo_with_file(
        filename: &str,
        content: &str,
    ) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        git_in(&root, &["init"]);
        git_in(&root, &["config", "user.email", "test@test.com"]);
        git_in(&root, &["config", "user.name", "Test"]);

        let file_path = root.join(filename);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file_path, content).unwrap();
        git_in(&root, &["add", "."]);
        git_in(&root, &["commit", "-m", "initial"]);

        let out = git_in(&root, &["rev-parse", "HEAD"]);
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert!(
            !sha.is_empty(),
            "fixture repo has no HEAD — the git environment leaked from a parallel test"
        );

        (dir, root, sha)
    }

    // 1. Write a map to a real git repo, load it back; assert found=true,
    //    summary matches, freshness verdict is fresh.
    #[test]
    fn test_write_then_load_returns_summary_and_fresh() {
        let app = test_app();
        let (dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// lib");
        let repo_path = root.to_string_lossy().to_string();

        let write_result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "core",
                "summary": "Core module overview",
                "head_sha": sha,
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        )
        .unwrap();
        assert_eq!(write_result["success"], true);

        let load_result = call_tool(
            &app,
            "code_map_load",
            &json!({
                "repo": repo_path,
                "area": "core",
            }),
        )
        .unwrap();

        assert_eq!(load_result["found"], true);
        assert_eq!(load_result["summary"], "Core module overview");
        assert_eq!(load_result["freshness"]["verdict"], "fresh");

        drop(dir);
    }

    // 2. Write map, change a tracked source file, load → stale + that file in changed_files.
    #[test]
    fn test_load_after_tracked_file_change_returns_stale() {
        let app = test_app();
        let (dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// v1");
        let repo_path = root.to_string_lossy().to_string();

        call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "core",
                "summary": "Original summary",
                "head_sha": sha,
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        )
        .unwrap();

        // Change the tracked file and commit
        std::fs::write(root.join("src/lib.rs"), "// v2 changed").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "update lib"])
            .current_dir(&root)
            .output()
            .unwrap();

        let load_result = call_tool(
            &app,
            "code_map_load",
            &json!({
                "repo": repo_path,
                "area": "core",
            }),
        )
        .unwrap();

        assert_eq!(load_result["found"], true);
        assert_eq!(load_result["freshness"]["verdict"], "stale");
        let changed = load_result["freshness"]["changed_files"]
            .as_array()
            .unwrap();
        assert!(
            changed.iter().any(|f| f.as_str() == Some("src/lib.rs")),
            "expected src/lib.rs in changed_files, got {:?}",
            changed
        );

        drop(dir);
    }

    // 3. Load (repo, "nonexistent-area") → found=false, verdict rescout_required.
    #[test]
    fn test_load_absent_map_returns_rescout() {
        let app = test_app();
        let (dir, root, _sha) = make_git_repo_with_file("src/lib.rs", "// lib");
        let repo_path = root.to_string_lossy().to_string();

        let load_result = call_tool(
            &app,
            "code_map_load",
            &json!({
                "repo": repo_path,
                "area": "nonexistent-area",
            }),
        )
        .unwrap();

        assert_eq!(load_result["found"], false);
        assert_eq!(load_result["freshness"]["verdict"], "rescout_required");

        drop(dir);
    }

    // 4. In read-only mode, code_map_write returns an error.
    #[test]
    fn test_write_mode_gated() {
        let app = test_app_readonly();

        let result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": "/some/repo",
                "area": "core",
                "summary": "Summary",
                "head_sha": "abc123",
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        );

        assert!(
            result.is_err(),
            "code_map_write must be blocked in read-only mode"
        );
    }

    // 5. source_files with absolute path → validation error before any git call.
    #[test]
    fn test_invalid_absolute_source_file_rejected() {
        let app = test_app();

        let result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": "/some/repo",
                "area": "core",
                "summary": "Summary",
                "head_sha": "deadbeef",
                "source_files": ["/etc/passwd"],
                "built_by": "test-agent",
            }),
        );

        assert!(result.is_err(), "absolute source_file must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("absolute")
                || err.contains("source_file")
                || err.contains("repo-relative"),
            "error message should mention the absolute path problem, got: {err}"
        );
    }

    // 6. source_files with parent traversal → validation error.
    #[test]
    fn test_invalid_parent_traversal_source_file_rejected() {
        let app = test_app();

        let result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": "/some/repo",
                "area": "core",
                "summary": "Summary",
                "head_sha": "deadbeef",
                "source_files": ["../outside.rs"],
                "built_by": "test-agent",
            }),
        );

        assert!(
            result.is_err(),
            "parent-traversal source_file must be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parent") || err.contains("source_file") || err.contains(".."),
            "error message should mention parent traversal, got: {err}"
        );
    }

    #[test]
    fn test_invalid_repo_path_rejected_before_write() {
        let app = test_app();
        let dir = tempfile::tempdir().unwrap();
        let missing_repo = dir.path().join("missing-repo");

        let result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": missing_repo,
                "area": "core",
                "summary": "Summary",
                "head_sha": "deadbeef",
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        );

        assert!(result.is_err(), "missing repo path must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("git worktree") || err.contains("existing"),
            "error message should mention the repo/worktree problem, got: {err}"
        );
    }

    #[test]
    fn test_source_files_rejects_non_string_entries() {
        let app = test_app();
        let (dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// lib");
        let repo_path = root.to_string_lossy().to_string();

        let result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "core",
                "summary": "Summary",
                "head_sha": sha,
                "source_files": ["src/lib.rs", 42],
                "built_by": "test-agent",
            }),
        );

        assert!(
            result.is_err(),
            "non-string source_files entries must be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("source_files") && err.contains("strings"),
            "error message should mention source_files strings, got: {err}"
        );

        drop(dir);
    }

    #[test]
    fn test_same_summary_different_areas_do_not_share_drawer() {
        let app = test_app();
        let (dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// lib");
        let repo_path = root.to_string_lossy().to_string();

        let core = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "core",
                "summary": "Shared summary",
                "head_sha": sha,
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        )
        .unwrap();
        let auth = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "auth",
                "summary": "Shared summary",
                "head_sha": sha,
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        )
        .unwrap();

        assert_ne!(
            core["drawer_id"], auth["drawer_id"],
            "area must participate in the drawer id"
        );

        call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "core",
                "summary": "Refreshed core summary",
                "head_sha": sha,
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        )
        .unwrap();

        let auth_after_core_refresh = call_tool(
            &app,
            "code_map_load",
            &json!({
                "repo": repo_path,
                "area": "auth",
            }),
        )
        .unwrap();

        assert_eq!(auth_after_core_refresh["found"], true);
        assert_eq!(auth_after_core_refresh["summary"], "Shared summary");

        drop(dir);
    }

    // head_sha must be a hex git object name — a non-hex value is rejected at
    // write time (before persisting a permanently-rescout map).
    #[test]
    fn test_write_rejects_non_hex_head_sha() {
        let app = test_app();
        let (dir, root, _sha) = make_git_repo_with_file("src/lib.rs", "// lib");
        let repo_path = root.to_string_lossy().to_string();

        let result = call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "core",
                "summary": "Summary",
                "head_sha": "HEAD", // non-hex ref name → rejected
                "source_files": ["src/lib.rs"],
                "built_by": "test-agent",
            }),
        );

        assert!(result.is_err(), "non-hex head_sha must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("head_sha") && err.contains("hex"),
            "error should mention head_sha hex requirement, got: {err}"
        );
        // Nothing must have been persisted.
        assert!(app.db.get_code_map(&repo_path, "core").unwrap().is_none());

        drop(dir);
    }

    // Read-path parity with the write-path repo validation: a load against a
    // non-existent / traversing repo path errors before any git runs.
    #[test]
    fn test_load_rejects_invalid_repo_path() {
        let app = test_app();
        let dir = tempfile::tempdir().unwrap();
        let missing_repo = dir.path().join("missing-repo");

        let result = call_tool(
            &app,
            "code_map_load",
            &json!({
                "repo": missing_repo,
                "area": "core",
            }),
        );

        assert!(
            result.is_err(),
            "missing repo path must be rejected on load"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("git worktree") || err.contains("existing"),
            "error should mention the repo/worktree problem, got: {err}"
        );

        let trav = call_tool(
            &app,
            "code_map_status",
            &json!({
                "repo": "/tmp/../etc",
                "area": "core",
            }),
        );
        assert!(
            trav.is_err(),
            "parent-traversal repo path must be rejected on status"
        );

        drop(dir);
    }

    // 7. After write, code_map_status returns freshness verdict without summary body.
    #[test]
    fn test_status_lightweight() {
        let app = test_app();
        let (dir, root, sha) = make_git_repo_with_file("main.rs", "fn main() {}");
        let repo_path = root.to_string_lossy().to_string();

        call_tool(
            &app,
            "code_map_write",
            &json!({
                "repo": repo_path,
                "area": "main",
                "summary": "Main module overview",
                "head_sha": sha,
                "source_files": ["main.rs"],
                "built_by": "test-agent",
            }),
        )
        .unwrap();

        let status_result = call_tool(
            &app,
            "code_map_status",
            &json!({
                "repo": repo_path,
                "area": "main",
            }),
        )
        .unwrap();

        assert_eq!(status_result["found"], true);
        // freshness must be present
        assert!(
            status_result.get("freshness").is_some(),
            "freshness must be present"
        );
        // summary must NOT be present (lightweight)
        assert!(
            status_result.get("summary").is_none(),
            "status must not include summary body"
        );
        // meta must NOT be present (lightweight)
        assert!(
            status_result.get("meta").is_none(),
            "status must not include meta"
        );

        drop(dir);
    }
}
