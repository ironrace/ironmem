use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use ironmem::config::{Config, EmbedMode, McpAccessMode};
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
