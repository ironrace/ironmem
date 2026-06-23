use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ironmem::config::{Config, EmbedMode, McpAccessMode};
use ironmem::db::schema::Database;
use ironmem::db::{NewTokenUsage, TaskOutcome};
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::json;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn base_command(home: &Path, db_path: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        // Smoke tests exercise the full write path; opt in explicitly now that
        // the binary default is read-only.
        .env("IRONMEM_MCP_MODE", "trusted");
    cmd
}

#[test]
fn cli_init_mine_serve_and_hook_smoke_test() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("README.md"),
        "# Workspace\n\nSmoke test content for mining.",
    )
    .unwrap();

    let init = base_command(&home, &db_path).arg("init").output().unwrap();
    assert!(init.status.success(), "init failed: {:?}", init);

    let mine = base_command(&home, &db_path)
        .arg("mine")
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(mine.status.success(), "mine failed: {:?}", mine);

    let mut serve = base_command(&home, &db_path)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = serve.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{}}})
        )
        .unwrap();
    }
    let output = serve.wait_with_output().unwrap();
    assert!(output.status.success(), "serve failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"protocolVersion\":\"2024-11-05\""));
    assert!(stdout.contains("total_drawers"));

    let session_start_payload = json!({
        "cwd": workspace,
        "session_id": "smoke-session"
    })
    .to_string();
    let mut hook_start = base_command(&home, &db_path)
        .arg("hook")
        .arg("session-start")
        .arg("--harness")
        .arg("codex")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook_start
        .stdin
        .as_mut()
        .unwrap()
        .write_all(session_start_payload.as_bytes())
        .unwrap();
    let hook_start_output = hook_start.wait_with_output().unwrap();
    assert!(hook_start_output.status.success());

    let transcript_path = workspace.join("transcript.jsonl");
    std::fs::write(
        &transcript_path,
        format!(
            "{}\n",
            json!({
                "message": {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "text",
                            "text": "Findings\n- High: transcript-derived review storage is missing in crates/ironmem/src/hook.rs:48\n- Medium: add an end-to-end smoke assertion\nPR #7"
                        }
                    ]
                }
            })
        ),
    )
    .unwrap();

    let stop_payload = json!({
        "cwd": workspace,
        "session_id": "smoke-session",
        "transcript_path": &transcript_path
    })
    .to_string();
    let mut hook_stop = base_command(&home, &db_path)
        .arg("hook")
        .arg("stop")
        .arg("--harness")
        .arg("codex")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook_stop
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stop_payload.as_bytes())
        .unwrap();
    let hook_stop_output = hook_stop.wait_with_output().unwrap();
    assert!(hook_stop_output.status.success());

    let app = App::new(Config {
        db_path,
        model_dir: temp.path().join("noop-model"),
        model_dir_explicit: true,
        state_dir: home.join(".ironrace-memory").join("hook_state"),
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    })
    .unwrap();
    let req: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc":"2.0",
        "id": 3,
        "method":"tools/call",
        "params":{"name":"diary_read","arguments":{"wing":"diary","limit":10}}
    }))
    .unwrap();
    let resp = dispatch(&app, &req).unwrap();
    let result = resp.result.unwrap();
    let body = result["content"][0]["text"].as_str().unwrap();
    let diary: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(
        diary["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["content"]
                .as_str()
                .unwrap_or_default()
                .contains("Hook stop ran")),
        "hook stop should persist a diary summary retrievable from the store"
    );

    let reviews = app
        .db
        .get_drawers(Some("reviews"), Some("pr-7"), 10)
        .unwrap();
    assert_eq!(reviews.len(), 1);
    assert!(reviews[0].content.contains("Findings"));
    assert!(reviews[0].source_file.ends_with("transcript.jsonl"));
}

#[test]
fn cli_dashboard_rejects_invalid_host() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    // Migrate a DB so startup gets past schema checks and fails only on host.
    {
        let db = Database::open(&db_path).unwrap();
        db.migrate().unwrap();
    }

    let out = base_command(&home, &db_path)
        .arg("dashboard")
        .arg("--host")
        .arg("not-an-ip")
        .arg("--port")
        .arg("0")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "dashboard with invalid host must exit non-zero: {out:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid host"),
        "stderr must mention invalid host: {stderr}"
    );
}

#[test]
fn cli_report_json_smoke_test() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("report.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    // Seed directly via the storage API: one merged task + one measured row.
    {
        let db = Database::open(&db_path).unwrap();
        db.migrate().unwrap();
        db.upsert_task_outcome(&TaskOutcome {
            task_tag: "issue-rep".into(),
            collab_session_id: Some("sess-rep".into()),
            started_at: Some("2026-06-01T00:00:00Z".into()),
            done_at: Some("2026-06-02T00:00:00Z".into()),
            outcome: Some("merged".into()),
            review_rounds: 1,
            fix_commits: 0,
            handoffs: 0,
            pr_url: Some("https://github.com/ironrace/ironmem/pull/100".into()),
        })
        .unwrap();
        db.insert_token_usage(&NewTokenUsage {
            ts: "2026-06-01T01:00:00Z".into(),
            source: "llm_rerank".into(),
            harness: "claude".into(),
            model: Some("claude-opus-4-8".into()),
            session_id: None,
            collab_session_id: Some("sess-rep".into()),
            collab_phase: Some("impl".into()),
            task_tag: None,
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: None,
            area: None,
        })
        .unwrap();
    }

    let report = base_command(&home, &db_path)
        .arg("report")
        .arg("--task")
        .arg("issue-rep")
        .arg("--since")
        .arg("2026-06-01T01:00:00+00:00")
        .arg("--json")
        .output()
        .unwrap();
    assert!(report.status.success(), "report failed: {:?}", report);

    let stdout = String::from_utf8(report.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("report --json must emit valid JSON ({e}): {stdout}"));
    assert!(
        value.get("baseline_ready").is_some(),
        "report JSON missing baseline_ready: {stdout}"
    );
    assert_eq!(
        value["generated_for"]["task"].as_str(),
        Some("issue-rep"),
        "report JSON missing task filter echo: {stdout}"
    );
    assert_eq!(
        value["generated_for"]["since"].as_str(),
        Some("2026-06-01T01:00:00Z"),
        "report JSON missing normalized since filter echo: {stdout}"
    );
    assert_eq!(
        value["tasks"][0]["task_key"].as_str(),
        Some("sess-rep"),
        "task_tag filter must resolve collab token rows: {stdout}"
    );
    assert!(
        value.get("headline").is_some(),
        "report JSON missing headline: {stdout}"
    );
}

// ── Helper: create a minimal git repo with one committed Rust file ──────────

fn make_git_repo_with_rust(root: &Path, rs_content: &str) {
    std::fs::create_dir_all(root).unwrap();
    Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(root)
        .output()
        .unwrap();
    std::fs::write(root.join("lib.rs"), rs_content).unwrap();
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(root)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(root)
        .output()
        .unwrap();
}

fn symbols_cmd(db_path: &PathBuf, home: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        .env("IRONMEM_MCP_MODE", "trusted");
    cmd
}

#[test]
fn cli_symbols_index_lookup_imports_neighbors_smoke() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("symbols.sqlite3");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();

    {
        let db = Database::open(&db_path).unwrap();
        db.migrate().unwrap();
    }

    make_git_repo_with_rust(
        &repo,
        "use std::collections::HashMap;\npub fn greet(name: &str) -> String { name.to_string() }\n",
    );

    // ── index ──────────────────────────────────────────────────────────────
    let index_out = symbols_cmd(&db_path, &home)
        .args(["symbols", "index", "--json", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    assert!(
        index_out.status.success(),
        "symbols index failed: {index_out:?}"
    );
    let index_json: serde_json::Value =
        serde_json::from_slice(&index_out.stdout).expect("symbols index --json must emit JSON");
    assert!(
        index_json["files_indexed"].as_u64().unwrap_or(0) >= 1,
        "should index at least 1 file: {index_json}"
    );
    assert!(
        index_json["symbols_inserted"].as_u64().unwrap_or(0) >= 1,
        "should insert at least 1 symbol: {index_json}"
    );
    assert!(
        index_json["imports_inserted"].as_u64().unwrap_or(0) >= 1,
        "should insert at least 1 import: {index_json}"
    );

    let relative_index_out = symbols_cmd(&db_path, &home)
        .current_dir(&repo)
        .args(["symbols", "index", "--json", "--repo", "."])
        .output()
        .unwrap();
    assert!(
        relative_index_out.status.success(),
        "symbols index must accept relative repo path '.': {relative_index_out:?}"
    );
    let relative_index_json: serde_json::Value = serde_json::from_slice(&relative_index_out.stdout)
        .expect("symbols index --json . must emit JSON");
    assert!(
        relative_index_json["files_skipped"].as_u64().unwrap_or(0) >= 1,
        "relative re-index should see unchanged indexed files: {relative_index_json}"
    );

    let repo_str = repo.to_string_lossy().to_string();

    // ── lookup ─────────────────────────────────────────────────────────────
    let lookup_out = symbols_cmd(&db_path, &home)
        .args(["symbols", "lookup", "--repo", &repo_str, "--json", "greet"])
        .output()
        .unwrap();
    assert!(
        lookup_out.status.success(),
        "symbols lookup failed: {lookup_out:?}"
    );
    let lookup_json: serde_json::Value =
        serde_json::from_slice(&lookup_out.stdout).expect("symbols lookup --json must emit JSON");
    let syms = lookup_json.as_array().expect("lookup must return array");
    assert!(
        syms.iter()
            .any(|s| s["name"].as_str() == Some("greet") && s["kind"].as_str() == Some("fn")),
        "lookup must find 'greet' fn: {lookup_json}"
    );
    // Verify shape: path, start_line, signature present.
    let greet = syms.iter().find(|s| s["name"] == "greet").unwrap();
    assert_eq!(greet["path"].as_str(), Some("lib.rs"));
    assert!(
        greet["start_line"].is_number(),
        "symbol must have start_line"
    );
    assert!(greet["start_col"].is_number(), "symbol must have start_col");
    assert_eq!(
        greet["signature"].as_str(),
        Some("pub fn greet(name: &str) -> String")
    );

    // ── imports ────────────────────────────────────────────────────────────
    let imports_out = symbols_cmd(&db_path, &home)
        .args([
            "symbols",
            "imports",
            "--repo",
            &repo_str,
            "--json",
            "std::collections",
        ])
        .output()
        .unwrap();
    assert!(
        imports_out.status.success(),
        "symbols imports failed: {imports_out:?}"
    );
    let imports_json: serde_json::Value =
        serde_json::from_slice(&imports_out.stdout).expect("symbols imports --json must emit JSON");
    let imps = imports_json.as_array().expect("imports must return array");
    assert!(
        imps.iter()
            .any(|i| i["module"].as_str() == Some("std::collections")),
        "imports must find std::collections: {imports_json}"
    );
    let imp = imps
        .iter()
        .find(|i| i["module"] == "std::collections")
        .unwrap();
    assert_eq!(imp["path"].as_str(), Some("lib.rs"));
    assert_eq!(imp["symbol"].as_str(), Some("HashMap"));
    assert_eq!(imp["raw"].as_str(), Some("use std::collections::HashMap;"));
    assert!(imp["line"].is_number(), "import must have line");

    // ── neighbors ──────────────────────────────────────────────────────────
    let neighbors_out = symbols_cmd(&db_path, &home)
        .args([
            "symbols",
            "neighbors",
            "--repo",
            &repo_str,
            "--json",
            "lib.rs",
        ])
        .output()
        .unwrap();
    assert!(
        neighbors_out.status.success(),
        "symbols neighbors failed: {neighbors_out:?}"
    );
    let neighbors_json: serde_json::Value = serde_json::from_slice(&neighbors_out.stdout)
        .expect("symbols neighbors --json must emit JSON");
    let edges = neighbors_json
        .as_array()
        .expect("neighbors must return array");
    // At least one import edge from lib.rs → std::collections.
    assert!(
        !edges.is_empty(),
        "neighbors for lib.rs must include import edges: {neighbors_json}"
    );
    let edge = edges
        .iter()
        .find(|e| {
            e["edge_kind"].as_str() == Some("import")
                && e["from_id"].as_str() == Some("lib.rs")
                && e["to_ref"].as_str() == Some("std::collections")
        })
        .expect("neighbors must include lib.rs import edge to std::collections");
    assert_eq!(edge["from_kind"].as_str(), Some("file"));
    assert_eq!(edge["to_kind"].as_str(), Some("module"));
    assert_eq!(edge["path"].as_str(), Some("lib.rs"));
    assert!(edge["line"].is_number(), "edge must include line");
}
