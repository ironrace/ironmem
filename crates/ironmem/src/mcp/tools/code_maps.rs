//! MCP tool handlers for lazy per-area code maps (issue #94).
//! Tools: `code_map_write`, `code_map_load`, `code_map_status`.

use serde_json::{json, Value};

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
        if f.starts_with('/') {
            return Err(MemoryError::Validation(format!(
                "source_file must be repo-relative, not absolute: {f}"
            )));
        }
        let path = std::path::Path::new(f);
        for component in path.components() {
            if component == std::path::Component::ParentDir {
                return Err(MemoryError::Validation(format!(
                    "source_file must not traverse parent directories: {f}"
                )));
            }
        }
        out.push(f.to_string());
    }
    Ok(out)
}

fn validate_repo(raw: &str) -> Result<String, MemoryError> {
    if raw.is_empty() {
        return Err(MemoryError::Validation("repo is required".into()));
    }
    let canonical = raw.trim_end_matches('/');
    // Require an absolute path to prevent git being invoked in an
    // unintended relative or traversal directory.
    if !canonical.starts_with('/') {
        return Err(MemoryError::Validation(format!(
            "repo must be an absolute path: {canonical}"
        )));
    }
    // Reject parent-component traversal (e.g. "/foo/../../etc").
    for component in std::path::Path::new(canonical).components() {
        if component == std::path::Component::ParentDir {
            return Err(MemoryError::Validation(format!(
                "repo must not contain parent-directory traversal: {canonical}"
            )));
        }
    }
    Ok(canonical.to_string())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub(super) fn handle_code_map_write(app: &App, args: &Value) -> Result<Value, MemoryError> {
    if app.is_warming_up() {
        return Ok(json!({
            "warming_up": true,
            "message": "Memory server is initializing. Please retry in a moment.",
        }));
    }

    // --- Validate inputs ---
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

    let source_files_raw: Vec<&str> = args
        .get("source_files")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let source_files = validate_source_files(&source_files_raw)?;

    let built_by = args
        .get("built_by")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MemoryError::Validation("built_by is required".into()))?;

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

    let drawer_id = generate_id(summary, &repo, "code-maps");
    let built_at = chrono::Utc::now().to_rfc3339();

    // --- Atomic transaction: insert drawer + upsert sidecar ---
    app.db.with_transaction(|tx| {
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
        Database::upsert_code_map_tx(
            tx,
            &repo,
            &area,
            &drawer_id,
            head_sha,
            &source_files,
            built_by,
            &built_at,
        )?;
        Ok(())
    })?;

    // Update live HNSW index
    app.insert_into_index(&drawer_id, &embedding)?;

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
            "freshness": {
                "verdict": "rescout_required",
                "reason": "no map found"
            }
        })),
        Some(map) => {
            let freshness = classify(&map, std::path::Path::new(&map.repo));
            let freshness_json = freshness_to_json(freshness);

            // Load drawer content
            let summary = app
                .db
                .get_drawer(&map.drawer_id)?
                .map(|d| d.content)
                .unwrap_or_default();

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
            "freshness": {
                "verdict": "rescout_required",
                "reason": "no map found"
            }
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

    // Set up a real git repo with a committed file and return (TempDir, root, sha).
    fn make_git_repo_with_file(
        filename: &str,
        content: &str,
    ) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .output()
            .unwrap();

        let file_path = root.join(filename);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file_path, content).unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&root)
            .output()
            .unwrap();

        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        let sha = String::from_utf8(out.stdout).unwrap().trim().to_string();

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

        let load_result = call_tool(
            &app,
            "code_map_load",
            &json!({
                "repo": "/some/repo",
                "area": "nonexistent-area",
            }),
        )
        .unwrap();

        assert_eq!(load_result["found"], false);
        assert_eq!(load_result["freshness"]["verdict"], "rescout_required");
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
                "head_sha": "abc123",
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
                "head_sha": "abc123",
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
