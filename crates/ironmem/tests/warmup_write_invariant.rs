//! Task 1 (RED test, root cause of `daemon_autospawn_race.rs` flakiness):
//! write-shaped MCP tool handlers silently no-op during warm-up.
//!
//! `handle_add_drawer`, `handle_diary_write`, and `handle_code_map_write` each
//! begin with `if app.is_warming_up() { return Ok(json!({"warming_up": true,
//! ...})) }` — a success-shaped response that performs NO write. At the wire
//! level, `mcp::server::dispatch` renders any `Ok(_)` from `tools::call_tool`
//! as an ordinary JSON-RPC success with no `isError` — so this warm-up no-op
//! is indistinguishable from a genuine successful write to a caller. That is
//! the root cause of 5 concurrent `add_drawer` callers over a shared,
//! still-warming daemon landing only 4/5 rows.
//!
//! `handle_search` also checks `is_warming_up()`, but it is READ-shaped and
//! its soft `{"warming_up": true, "results": []}` body is correct, existing
//! behavior that must NOT change.
//!
//! This test drives each tool through the real wire-level `dispatch` entry
//! point (not the handler function directly) against a single in-process
//! `App::open_for_test()` forced into the warm-up state, and asserts that
//! each write tool's response is either a completed-write acknowledgement or
//! an explicit `isError: true` — never the soft `warming_up` no-op body.
//!
//! Expected to FAIL on HEAD for `add_drawer`, `diary_write`, and
//! `code_map_write` (all three currently return the soft body as if it were a
//! success), while `search`'s case passes unchanged. This is the documented
//! RED step of TDD for the daemon-autospawn-race root-cause fix; the write
//! handlers are not touched by this task.

use std::sync::atomic::Ordering;

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::{json, Value};

fn request(name: &str, arguments: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    }))
    .expect("request fixture must deserialize")
}

/// Drive a tool call through the real wire-level `dispatch` entrypoint and
/// return `(is_error, payload)`, where `payload` is the tool's inner JSON
/// body decoded from `result.content[0].text` — the same path a real MCP
/// client observes.
fn call_tool_raw(app: &App, name: &str, args: Value) -> (bool, Value) {
    let req = request(name, args);
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    assert!(
        resp.error.is_none(),
        "unexpected RPC-level error for tool {name}"
    );
    let result = resp.result.expect("tool result must be present");
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    let payload: Value = serde_json::from_str(text).expect("tool response text must be valid JSON");
    (is_error, payload)
}

/// Force the `App` into the warm-up state that `is_warming_up()` reads.
/// `App::open_for_test()` starts with `memory_ready = true`; storing `false`
/// here reproduces the daemon's real warm-up window without needing a real
/// background init thread.
fn force_warming_up(app: &App) {
    app.memory_ready.store(false, Ordering::Relaxed);
    assert!(app.is_warming_up(), "failed to force warm-up state");
}

/// Mirrors `code_maps.rs`'s own test helper: a minimal real git repo so
/// `code_map_write`'s `repo`/`head_sha` validation (which shells out to git)
/// can succeed.
fn make_git_repo_with_file(
    filename: &str,
    contents: &str,
) -> (tempfile::TempDir, std::path::PathBuf, String) {
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
    std::fs::write(&file_path, contents).unwrap();
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

/// The RED assertion shared by all three write-tool tests: a warm-up no-op
/// body (`{"warming_up": true, ...}`) must never be returned as if it were a
/// completed write. It is acceptable only when paired with `isError: true`
/// (a hard rejection is a distinguishable, honest failure); as a plain
/// success it is invisible to the caller and silently drops the write.
fn assert_write_completed_or_errored(tool: &str, is_error: bool, payload: &Value) {
    let warming_up_body = payload.get("warming_up").and_then(Value::as_bool) == Some(true);
    assert!(
        !warming_up_body || is_error,
        "{tool}: warm-up no-op body returned as an ordinary success \
         (isError={is_error}, payload={payload}) — a write-shaped tool must not \
         silently no-op during warm-up; it must either complete the write or \
         surface isError:true"
    );
}

#[test]
fn add_drawer_does_not_silently_noop_during_warmup() {
    let app = App::open_for_test().unwrap();
    force_warming_up(&app);

    let (is_error, payload) = call_tool_raw(
        &app,
        "add_drawer",
        json!({ "content": "warmup race test content", "wing": "ironrace-memory" }),
    );

    assert_write_completed_or_errored("add_drawer", is_error, &payload);
}

#[test]
fn diary_write_does_not_silently_noop_during_warmup() {
    let app = App::open_for_test().unwrap();
    force_warming_up(&app);

    let (is_error, payload) = call_tool_raw(
        &app,
        "diary_write",
        json!({ "content": "warmup race diary entry" }),
    );

    assert_write_completed_or_errored("diary_write", is_error, &payload);
}

#[test]
fn code_map_write_does_not_silently_noop_during_warmup() {
    let app = App::open_for_test().unwrap();
    force_warming_up(&app);

    let (_dir, root, sha) = make_git_repo_with_file("src/lib.rs", "// lib");

    let (is_error, payload) = call_tool_raw(
        &app,
        "code_map_write",
        json!({
            "repo": root.to_string_lossy(),
            "area": "core",
            "summary": "warmup race code map summary",
            "head_sha": sha,
            "source_files": ["src/lib.rs"],
            "built_by": "test",
        }),
    );

    assert_write_completed_or_errored("code_map_write", is_error, &payload);
}

/// Companion assertion: `search` is READ-shaped, and its soft
/// `{"warming_up": true, "results": []}` body during warm-up is CORRECT
/// existing behavior — it must keep passing, unlike the three write tools
/// above.
#[test]
fn search_is_allowed_to_return_soft_warmup_body() {
    let app = App::open_for_test().unwrap();
    force_warming_up(&app);

    let (is_error, payload) = call_tool_raw(&app, "search", json!({ "query": "anything" }));

    assert!(!is_error, "search must not error during warm-up");
    assert_eq!(
        payload["warming_up"].as_bool(),
        Some(true),
        "search's soft warm-up body must be preserved: {payload}"
    );
    assert_eq!(
        payload["results"].as_array().map(Vec::len),
        Some(0),
        "search's warm-up body must still report empty results: {payload}"
    );
}
