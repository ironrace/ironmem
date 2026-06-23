//! MCP tool handlers for the local symbol/import graph index.
//!
//! Tools:
//! - `symbol_graph_index`  — index a repo (write-gated)
//! - `symbol_graph_lookup` — look up symbols by name
//! - `symbol_graph_imports`   — look up imports by file or module
//! - `symbol_graph_neighbors` — look up edges by symbol id or file

use serde_json::{json, Value};

use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::symbol_graph::{
    canonicalize_repo, index_repo, lookup_imports, lookup_neighbors, lookup_symbols,
};

/// Hard limit on results returned by any read tool — prevents unbounded responses.
const MAX_RESULTS: usize = 100;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, MemoryError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| MemoryError::Validation(format!("'{key}' is required")))
}

fn clamp_limit(args: &Value, cap: usize) -> usize {
    args.get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(50)
        .min(cap)
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `symbol_graph_index` — walk and (re-)index a repo's Rust + Python sources.
/// Write-gated: only callable when `IRONMEM_MCP_MODE != read-only/restricted`.
pub(super) fn handle_symbol_graph_index(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_raw = required_str(args, "repo")?;
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    // Canonicalize and validate repo path; map FS/git errors to generic messages.
    let canonical = canonicalize_repo(repo_raw).map_err(|e| {
        // Log detail server-side; return generic message to client.
        eprintln!("[symbol-graph-mcp] index repo validation failed: {e}");
        MemoryError::Validation(
            "repo must be an accessible git worktree (absolute path required)".into(),
        )
    })?;

    let result = index_repo(&app.db, &canonical, force).map_err(|e| {
        eprintln!("[symbol-graph-mcp] index_repo error: {e}");
        MemoryError::Validation("symbol_graph_index: indexing failed (check server logs)".into())
    })?;

    Ok(json!({
        "repo": canonical,
        "files_indexed": result.files_indexed,
        "files_skipped": result.files_skipped,
        "files_purged": result.files_purged,
        "symbols_inserted": result.symbols_inserted,
        "imports_inserted": result.imports_inserted,
        "edges_inserted": result.edges_inserted,
        "head_resolved": result.head_resolved,
        "head_sha": result.head_sha,
    }))
}

/// `symbol_graph_lookup` — look up symbol declarations by name.
pub(super) fn handle_symbol_graph_lookup(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_raw = required_str(args, "repo")?;
    let query = required_str(args, "query")?;
    let kind = args.get("kind").and_then(|v| v.as_str());
    let limit = clamp_limit(args, MAX_RESULTS);

    let canonical = canonicalize_repo(repo_raw).map_err(|e| {
        eprintln!("[symbol-graph-mcp] lookup repo validation failed: {e}");
        MemoryError::Validation(
            "repo must be an accessible git worktree (absolute path required)".into(),
        )
    })?;

    let symbols = lookup_symbols(&app.db, &canonical, query, kind, Some(limit))?;

    let items: Vec<Value> = symbols
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "qualified_name": s.qualified_name,
                "kind": s.kind,
                "path": s.path,
                "language": s.language,
                "visibility": s.visibility,
                "signature": s.signature,
                "start_line": s.start_line,
                "start_col": s.start_col,
                "end_line": s.end_line,
                "parent_id": s.parent_id,
                "confidence": s.confidence,
            })
        })
        .collect();

    Ok(json!({ "symbols": items, "count": items.len() }))
}

/// `symbol_graph_imports` — look up imports by file path or module name.
pub(super) fn handle_symbol_graph_imports(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_raw = required_str(args, "repo")?;
    let query = required_str(args, "query")?;
    let limit = clamp_limit(args, MAX_RESULTS);

    let canonical = canonicalize_repo(repo_raw).map_err(|e| {
        eprintln!("[symbol-graph-mcp] imports repo validation failed: {e}");
        MemoryError::Validation(
            "repo must be an accessible git worktree (absolute path required)".into(),
        )
    })?;

    let imports = lookup_imports(&app.db, &canonical, query, Some(limit))?;

    let items: Vec<Value> = imports
        .iter()
        .map(|i| {
            json!({
                "id": i.id,
                "path": i.path,
                "language": i.language,
                "module": i.module,
                "symbol": i.symbol,
                "alias": i.alias,
                "raw": i.raw,
                "line": i.line,
                "confidence": i.confidence,
            })
        })
        .collect();

    Ok(json!({ "imports": items, "count": items.len() }))
}

/// `symbol_graph_neighbors` — look up edges by symbol id or file path.
pub(super) fn handle_symbol_graph_neighbors(app: &App, args: &Value) -> Result<Value, MemoryError> {
    let repo_raw = required_str(args, "repo")?;
    let query = required_str(args, "query")?;
    let limit = clamp_limit(args, MAX_RESULTS);

    let canonical = canonicalize_repo(repo_raw).map_err(|e| {
        eprintln!("[symbol-graph-mcp] neighbors repo validation failed: {e}");
        MemoryError::Validation(
            "repo must be an accessible git worktree (absolute path required)".into(),
        )
    })?;

    let edges = lookup_neighbors(&app.db, &canonical, query, Some(limit))?;

    let items: Vec<Value> = edges
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "from_kind": e.from_kind,
                "from_id": e.from_id,
                "to_kind": e.to_kind,
                "to_ref": e.to_ref,
                "edge_kind": e.edge_kind,
                "path": e.path,
                "line": e.line,
                "confidence": e.confidence,
            })
        })
        .collect();

    Ok(json!({ "edges": items, "count": items.len() }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpAccessMode;
    use crate::mcp::app::App;
    use crate::mcp::tools::call_tool;
    use serde_json::json;
    use std::process::Command;
    use std::sync::Arc;

    fn test_app() -> Arc<App> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(App::open_for_test().unwrap())
    }

    fn test_app_readonly() -> Arc<App> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(App::open_for_test_with_mode(McpAccessMode::ReadOnly).unwrap())
    }

    fn make_git_repo(content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        Command::new("git")
            .args(["init", "-b", "main"])
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
        std::fs::write(root.join("lib.rs"), content).unwrap();
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(&root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .output()
            .unwrap();
        let repo_path = dir.path().to_string_lossy().to_string();
        (dir, repo_path)
    }

    // 1. Index then lookup returns the symbol.
    #[test]
    fn test_index_then_lookup_returns_symbol() {
        let app = test_app();
        let (dir, repo) = make_git_repo("pub fn my_fn() {}\n");

        let idx = call_tool(&app, "symbol_graph_index", &json!({ "repo": repo })).unwrap();
        assert!(idx["files_indexed"].as_u64().unwrap_or(0) >= 1);

        let lookup = call_tool(
            &app,
            "symbol_graph_lookup",
            &json!({ "repo": repo, "query": "my_fn" }),
        )
        .unwrap();
        let syms = lookup["symbols"].as_array().unwrap();
        assert!(
            syms.iter().any(|s| s["name"] == "my_fn"),
            "lookup must find my_fn: {lookup}"
        );
        // Shape: path, start_line, kind
        let sym = syms.iter().find(|s| s["name"] == "my_fn").unwrap();
        assert!(sym["path"].is_string());
        assert!(sym["start_line"].is_number());
        assert_eq!(sym["kind"], "fn");

        drop(dir);
    }

    // 2. Index then imports returns the import.
    #[test]
    fn test_index_then_imports_returns_import() {
        let app = test_app();
        let (dir, repo) = make_git_repo("use std::collections::HashMap;\npub fn f() {}\n");

        call_tool(&app, "symbol_graph_index", &json!({ "repo": repo })).unwrap();

        let imp_result = call_tool(
            &app,
            "symbol_graph_imports",
            &json!({ "repo": repo, "query": "std::collections" }),
        )
        .unwrap();
        let imps = imp_result["imports"].as_array().unwrap();
        assert!(
            imps.iter().any(|i| i["module"] == "std::collections"),
            "imports must find std::collections: {imp_result}"
        );
        let imp = imps
            .iter()
            .find(|i| i["module"] == "std::collections")
            .unwrap();
        assert!(imp["path"].is_string());
        assert!(imp["line"].is_number());

        drop(dir);
    }

    // 3. Index then neighbors returns at least one import edge.
    #[test]
    fn test_index_then_neighbors_returns_edges() {
        let app = test_app();
        let (dir, repo) = make_git_repo("use std::io;\npub fn g() {}\n");

        call_tool(&app, "symbol_graph_index", &json!({ "repo": repo })).unwrap();

        let nb_result = call_tool(
            &app,
            "symbol_graph_neighbors",
            &json!({ "repo": repo, "query": "lib.rs" }),
        )
        .unwrap();
        let edges = nb_result["edges"].as_array().unwrap();
        assert!(
            !edges.is_empty(),
            "neighbors must return edges: {nb_result}"
        );
        let edge = &edges[0];
        assert!(edge["from_id"].is_string());
        assert!(edge["to_ref"].is_string());
        assert!(edge["edge_kind"].is_string());

        drop(dir);
    }

    // 4. symbol_graph_index is blocked in ReadOnly mode.
    #[test]
    fn test_index_blocked_in_readonly_mode() {
        let app = test_app_readonly();
        let (dir, repo) = make_git_repo("pub fn h() {}\n");

        let result = call_tool(&app, "symbol_graph_index", &json!({ "repo": repo }));
        assert!(
            result.is_err(),
            "symbol_graph_index must be blocked in read-only mode"
        );

        drop(dir);
    }

    // 5. Read tools (lookup, imports, neighbors) are allowed in ReadOnly mode
    //    after indexing from a Trusted app.
    #[test]
    fn test_read_tools_allowed_in_readonly_mode() {
        let trusted = test_app();
        let (dir, repo) = make_git_repo("pub fn ronly() {}\n");

        // Index via trusted app.
        call_tool(&trusted, "symbol_graph_index", &json!({ "repo": repo })).unwrap();

        // Now switch to a read-only app sharing the same DB.
        let readonly = test_app_readonly();

        let lookup = call_tool(
            &readonly,
            "symbol_graph_lookup",
            &json!({ "repo": repo, "query": "ronly" }),
        );
        assert!(
            lookup.is_ok(),
            "symbol_graph_lookup must be allowed in read-only mode: {lookup:?}"
        );

        let imports = call_tool(
            &readonly,
            "symbol_graph_imports",
            &json!({ "repo": repo, "query": "lib.rs" }),
        );
        assert!(
            imports.is_ok(),
            "symbol_graph_imports must be allowed in read-only mode: {imports:?}"
        );

        let neighbors = call_tool(
            &readonly,
            "symbol_graph_neighbors",
            &json!({ "repo": repo, "query": "lib.rs" }),
        );
        assert!(
            neighbors.is_ok(),
            "symbol_graph_neighbors must be allowed in read-only mode: {neighbors:?}"
        );

        drop(dir);
    }

    // 6. Traversal path in repo arg is rejected.
    #[test]
    fn test_traversal_repo_rejected() {
        let app = test_app();

        let result = call_tool(
            &app,
            "symbol_graph_index",
            &json!({ "repo": "/tmp/../etc" }),
        );
        assert!(
            result.is_err(),
            "traversal repo path must be rejected: {result:?}"
        );
    }

    // 7. Explicit limit is enforced — results never exceed MAX_RESULTS.
    #[test]
    fn test_limit_enforced() {
        let app = test_app();
        let (dir, repo) = make_git_repo("pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n");

        call_tool(&app, "symbol_graph_index", &json!({ "repo": repo })).unwrap();

        // Request limit=1 — must cap at 1.
        let result = call_tool(
            &app,
            "symbol_graph_lookup",
            &json!({ "repo": repo, "query": "a", "limit": 1 }),
        )
        .unwrap();
        let syms = result["symbols"].as_array().unwrap();
        assert!(syms.len() <= 1, "limit=1 must cap results: {result}");

        // Request limit=9999 — capped at MAX_RESULTS (100).
        let result_big = call_tool(
            &app,
            "symbol_graph_lookup",
            &json!({ "repo": repo, "query": "a", "limit": 9999 }),
        )
        .unwrap();
        let syms_big = result_big["symbols"].as_array().unwrap();
        assert!(
            syms_big.len() <= MAX_RESULTS,
            "limit must be capped at MAX_RESULTS: {result_big}"
        );

        drop(dir);
    }
}
