//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests call `dispatch` directly with an in-memory App (noop embedder,
//! no ONNX model required) and assert on the JSON-RPC response shape.

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(args: &[&str], cwd: &Path) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command must run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output must be valid utf-8")
        .trim()
        .to_string()
}

fn write_file(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("fixture file must be writable");
}

fn commit_file(cwd: &Path, filename: &str, contents: &str, message: &str) -> String {
    write_file(&cwd.join(filename), contents);
    git(&["add", filename], cwd);
    git(&["commit", "-m", message], cwd);
    git(&["rev-parse", "HEAD"], cwd)
}

fn git_repo_fixture() -> (tempfile::TempDir, PathBuf, String, String, String, String) {
    let temp = tempfile::tempdir().expect("temp repo must be creatable");
    let repo_path = temp.path().to_path_buf();
    git(&["init"], &repo_path);
    git(&["config", "user.name", "Ironmem Test"], &repo_path);
    git(&["config", "user.email", "ironmem@example.com"], &repo_path);

    let base_sha = commit_file(&repo_path, "branch.txt", "base\n", "base commit");
    let head_sha = commit_file(
        &repo_path,
        "branch.txt",
        "review start\n",
        "review start commit",
    );
    let descendant_sha = commit_file(
        &repo_path,
        "branch.txt",
        "review fix\n",
        "review fix commit",
    );

    git(&["checkout", "-b", "drift", &base_sha], &repo_path);
    let drift_sha = commit_file(&repo_path, "branch.txt", "drift\n", "drift commit");

    (
        temp,
        repo_path,
        base_sha,
        head_sha,
        descendant_sha,
        drift_sha,
    )
}

fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

fn call_tool(app: &App, name: &str, args: serde_json::Value) -> serde_json::Value {
    let req = request("tools/call", json!({ "name": name, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    assert!(resp.error.is_none(), "unexpected RPC error for tool {name}");
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    serde_json::from_str(text).expect("tool response text must be valid JSON")
}

/// Tool errors surface as an `isError: true` success response carrying a
/// JSON error string, not as the JSON-RPC `error` field. Return the error
/// message so callers can assert on its contents.
fn call_tool_expect_error(app: &App, name: &str, args: serde_json::Value) -> String {
    let req = request("tools/call", json!({ "name": name, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    let result = resp.result.expect("tool result must be present");
    assert_eq!(
        result["isError"], true,
        "expected tool error for {name}, got success: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap_or(json!({}));
    parsed["error"].as_str().unwrap_or(text).to_string()
}

#[test]
fn initialize_returns_capabilities() {
    let app = App::open_for_test().unwrap();
    let req = request("initialize", json!({}));
    let resp = dispatch(&app, &req).expect("initialize must return a response");

    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(result["serverInfo"]["name"], "ironmem");
}

#[test]
fn tools_list_contains_required_tools() {
    let app = App::open_for_test().unwrap();
    let req = request("tools/list", json!({}));
    let resp = dispatch(&app, &req).expect("tools/list must return a response");

    assert!(resp.error.is_none());
    let tools = resp.result.unwrap()["tools"]
        .as_array()
        .cloned()
        .expect("result.tools must be an array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for required in &[
        "status",
        "search",
        "list_wings",
        "kg_stats",
        "add_drawer",
        "diary_write",
        "collab_start_code_review",
    ] {
        assert!(
            names.contains(required),
            "missing required tool: {required}"
        );
    }
}

#[test]
fn tools_list_read_only_mode_excludes_write_tools() {
    use ironmem::config::McpAccessMode;

    let app = App::open_for_test_with_mode(McpAccessMode::ReadOnly).unwrap();
    let req = request("tools/list", json!({}));
    let resp = dispatch(&app, &req).unwrap();

    let tools = resp.result.unwrap()["tools"].as_array().cloned().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for blocked in &["add_drawer", "delete_drawer", "diary_write"] {
        assert!(
            !names.contains(blocked),
            "write tool should be absent in read-only mode: {blocked}"
        );
    }
    // Read tools still present
    assert!(names.contains(&"status"));
    assert!(names.contains(&"search"));
}

#[test]
fn status_returns_expected_shape() {
    let app = App::open_for_test().unwrap();
    let status = call_tool(&app, "status", json!({}));

    assert!(
        status["total_drawers"].is_number(),
        "total_drawers must be a number"
    );
    assert!(status["wings"].is_object(), "wings must be an object");
    assert!(
        status["knowledge_graph"].is_object(),
        "knowledge_graph must be an object"
    );
    let protocol = status["memory_protocol"].as_str().unwrap_or("");
    assert!(
        !protocol.is_empty(),
        "memory_protocol must be a non-empty string"
    );
}

#[test]
fn unknown_method_returns_method_not_found() {
    let app = App::open_for_test().unwrap();
    let req = request("nonexistent/method", json!({}));
    let resp = dispatch(&app, &req).expect("unknown method must return a response");

    let err = resp.error.expect("unknown method must return an error");
    assert_eq!(err.code, -32601);
}

#[test]
fn kg_add_and_query_round_trip() {
    let app = App::open_for_test().unwrap();

    // Add a triple
    let add = call_tool(
        &app,
        "kg_add",
        json!({ "subject": "rust", "predicate": "is-a", "object": "language" }),
    );
    assert_eq!(add["success"], true);

    // Query it back
    let query = call_tool(&app, "kg_query", json!({ "entity": "rust" }));
    let triples = query["triples"]
        .as_array()
        .expect("triples must be an array");
    assert!(
        !triples.is_empty(),
        "query should return the inserted triple"
    );
}

#[test]
fn collab_happy_path_locks_via_mcp_handlers() {
    let app = App::open_for_test().unwrap();

    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "Claude first draft"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex first draft"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanSynthesisPending");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "Merged canonical plan"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanCodexReviewPending");
    let canonical_hash = status["canonical_plan_hash"].as_str().unwrap().to_string();

    call_tool(
        &app,
        "collab_approve",
        json!({
            "session_id": session_id,
            "agent": "codex",
            "content_hash": canonical_hash
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanClaudeFinalizePending");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({
                "plan": "Final locked plan",
                "codex_still_objects": false
            }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanLocked");
}

#[test]
fn collab_send_rejects_non_owner_before_dispatch() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "c"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "x"
        }),
    );

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanSynthesisPending");
    assert_eq!(status["current_owner"], "claude");

    // Codex tries to send while claude is the owner → rejected upstream.
    let msg = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "canonical",
            "content": "hostile canonical"
        }),
    );
    assert!(msg.contains("not your turn"), "msg={msg}");
    assert!(
        msg.contains("claude"),
        "expected owner in error, got: {msg}"
    );
}

#[test]
fn collab_send_allows_either_agent_during_parallel_drafts() {
    // PlanParallelDrafts is exempt — current_owner there is a "next-expected"
    // hint, not a hard lock, and the blind-draft protocol lets whichever
    // agent is ready submit first.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    // Fresh session defaults to current_owner=claude, yet codex is still
    // allowed to submit its draft first.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "codex goes first"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanParallelDrafts");
}

#[test]
fn collab_request_changes_loops_back_to_synthesis_and_locks_after_revision() {
    let app = App::open_for_test().unwrap();

    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "Claude first draft"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex first draft"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "Merged canonical v1"
        }),
    );

    // Wrong hash on approve is rejected (canonical_plan_hash mismatch).
    let bad_approve = call_tool(
        &app,
        "collab_approve",
        json!({
            "session_id": session_id,
            "agent": "codex",
            "content_hash": "deadbeef"
        }),
    );
    assert!(bad_approve["error"]
        .as_str()
        .unwrap_or("")
        .contains("content_hash does not match canonical_plan_hash"));

    // Codex requests changes → revision round 1; phase returns to synthesis so
    // Claude can revise the canonical plan.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "request_changes" }).to_string()
        }),
    );
    let status_after_rc = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status_after_rc["phase"], "PlanSynthesisPending");
    assert_eq!(status_after_rc["current_owner"], "claude");
    assert_eq!(status_after_rc["review_round"], 1);

    // Claude publishes a revised canonical; Codex now approves. After approval
    // Claude publishes the final plan and the session locks.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "Merged canonical v2"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string()
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": "Merged canonical v2" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanLocked");
    assert_eq!(status["final_plan_hash"], status["canonical_plan_hash"]);
    // A fresh agent joining at PlanLocked must be able to pull the plan text
    // back without having to recv its own previously-sent (and peer-acked)
    // outbound message. collab_status surfaces the latest canonical/final
    // content alongside the hashes.
    assert_eq!(status["canonical_plan"], "Merged canonical v2");
    assert_eq!(
        status["final_plan"],
        json!({ "plan": "Merged canonical v2" }).to_string()
    );
}

#[test]
fn collab_status_omits_plan_text_before_plan_is_sent() {
    let app = App::open_for_test().unwrap();

    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert!(
        status.get("canonical_plan").is_none(),
        "canonical_plan must be absent before any canonical is published"
    );
    assert!(
        status.get("final_plan").is_none(),
        "final_plan must be absent before PlanLocked"
    );
}

#[test]
fn collab_two_rounds_of_request_changes_force_finalize() {
    let app = App::open_for_test().unwrap();

    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    // Submit both drafts.
    for (sender, content) in [("claude", "cdraft"), ("codex", "xdraft")] {
        call_tool(
            &app,
            "collab_send",
            json!({
                "session_id": session_id,
                "sender": sender,
                "topic": "draft",
                "content": content
            }),
        );
    }

    // Round 1: canonical → request_changes (back to synthesis).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "v1"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "request_changes" }).to_string()
        }),
    );

    // Round 2: canonical → request_changes again. Revision cap hit, so we
    // must advance to PlanClaudeFinalizePending (Claude gets the last word).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "v2"
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "request_changes" }).to_string()
        }),
    );

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanClaudeFinalizePending");
    assert_eq!(status["review_round"], 2);

    // Claude publishes final despite Codex's objection; session locks.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": "Claude's last word" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanLocked");
}

#[test]
fn collab_start_with_task_roundtrips_via_status() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "task": "design a landing page"
        }),
    );
    assert_eq!(started["task"], "design a landing page");
    let session_id = started["session_id"].as_str().unwrap();

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["task"], "design a landing page");
    assert_eq!(status["review_round"], 0);
    assert!(status["ended_at"].is_null());
}

#[test]
fn collab_start_code_review_roundtrips_via_status() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/landing-page",
            "base_sha": "abc123",
            "head_sha": "def456",
            "initiator": "claude",
            "task": "review landing page branch"
        }),
    );
    assert_eq!(started["task"], "review landing page branch");
    let session_id = started["session_id"].as_str().unwrap();

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["task"], "review landing page branch");
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["base_sha"], "abc123");
    assert_eq!(status["last_head_sha"], "def456");
    assert!(status["task_list"].is_null());
}

#[test]
fn collab_start_code_review_rejects_codex_initiator() {
    let app = App::open_for_test().unwrap();
    let error = call_tool_expect_error(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/landing-page",
            "base_sha": "abc123",
            "head_sha": "def456",
            "initiator": "codex",
            "task": "review landing page branch"
        }),
    );
    assert!(error.contains("initiator must be 'claude'"));
}

#[test]
fn collab_recv_blocks_draft_peek_before_own_draft_submitted() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    // Claude submits first.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "claude draft"
        }),
    );

    // Codex must NOT be able to read Claude's draft before submitting its own.
    let peek = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex" }),
    );
    let messages = peek["messages"].as_array().unwrap();
    assert!(
        messages.is_empty(),
        "drafts must be hidden during PlanParallelDrafts until receiver submits its own"
    );

    // After Codex submits its own draft, the phase advances and Codex can
    // read Claude's draft.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "codex draft"
        }),
    );
    let peek = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex" }),
    );
    let messages = peek["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"], "claude draft");
}

#[test]
fn collab_wait_my_turn_returns_immediately_when_owner() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    // Fresh session: current_owner=claude, PlanParallelDrafts.
    let start = std::time::Instant::now();
    let resp = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": "claude", "timeout_secs": 5 }),
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert_eq!(resp["is_my_turn"], true);
    assert_eq!(resp["phase"], "PlanParallelDrafts");
    assert_eq!(resp["current_owner"], "claude");
    assert_eq!(resp["session_ended"], false);
}

#[test]
fn collab_end_blocks_subsequent_writes() {
    let app = App::open_for_test().unwrap();
    // Drive to PlanLocked so collab_end is actually allowed — calling it
    // during active planning is rejected by the contract tested in
    // `collab_end_rejected_in_active_planning_phase`.
    let session_id = drive_to_plan_locked(&app, "fp");

    let ended = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert_eq!(ended["ok"], true);

    // Subsequent send must fail because the session has ended.
    let blocked = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            // Payload values are irrelevant — the session-ended gate must
            // reject before parsing.
            "content": task_list_payload("unused_plan_hash", "unused_base", "unused_head", 1)
        }),
    );
    assert!(blocked["error"]
        .as_str()
        .unwrap_or("")
        .contains("has ended"));

    // wait_my_turn must surface session_ended=true so the agent loop exits.
    let wait = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": &session_id, "agent": "claude", "timeout_secs": 1 }),
    );
    assert_eq!(wait["session_ended"], true);
    assert_eq!(wait["is_my_turn"], false);
}

#[test]
fn collab_end_rejected_in_active_planning_phase() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();

    // Fresh session → PlanParallelDrafts. end must be rejected.
    let blocked = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    assert!(
        blocked["error"]
            .as_str()
            .unwrap_or("")
            .contains("active phase PlanParallelDrafts"),
        "expected PlanParallelDrafts rejection, got: {blocked}"
    );

    // Advance to PlanSynthesisPending — still an active planning phase.
    for (sender, content) in [("claude", "cdraft"), ("codex", "xdraft")] {
        call_tool(
            &app,
            "collab_send",
            json!({
                "session_id": &session_id,
                "sender": sender,
                "topic": "draft",
                "content": content
            }),
        );
    }
    let blocked = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    assert!(
        blocked["error"]
            .as_str()
            .unwrap_or("")
            .contains("active phase PlanSynthesisPending"),
        "expected PlanSynthesisPending rejection, got: {blocked}"
    );

    // Advance to PlanCodexReviewPending → PlanClaudeFinalizePending.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "canonical v1"
        }),
    );
    let blocked = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &session_id, "agent": "codex" }),
    );
    assert!(
        blocked["error"]
            .as_str()
            .unwrap_or("")
            .contains("active phase PlanCodexReviewPending"),
        "expected PlanCodexReviewPending rejection, got: {blocked}"
    );

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string()
        }),
    );
    let blocked = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    assert!(
        blocked["error"]
            .as_str()
            .unwrap_or("")
            .contains("active phase PlanClaudeFinalizePending"),
        "expected PlanClaudeFinalizePending rejection, got: {blocked}"
    );

    // Reach PlanLocked — now end is allowed.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": "fp" }).to_string()
        }),
    );
    let ended = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    assert_eq!(ended["ok"], true);
}

// ── v2 coding-loop E2E tests ────────────────────────────────────────────────

/// Drive a fresh session all the way to PlanLocked via MCP handlers and
/// return `(session_id, final_plan_text)` so callers can assemble valid
/// `task_list` payloads (the state machine rejects a mismatched `plan_hash`).
fn drive_to_plan_locked(app: &App, final_plan: &str) -> String {
    drive_to_plan_locked_with_implementer(app, final_plan, None)
}

/// Same as `drive_to_plan_locked` but threads through the optional
/// `implementer` field. `None` keeps the historical default (`"claude"`).
fn drive_to_plan_locked_with_implementer(
    app: &App,
    final_plan: &str,
    implementer: Option<&str>,
) -> String {
    let mut start_args = json!({
        "repo_path": "/repo",
        "branch": "main",
        "initiator": "claude"
    });
    if let Some(value) = implementer {
        start_args["implementer"] = json!(value);
    }
    let started = call_tool(app, "collab_start", start_args);
    let session_id = started["session_id"].as_str().unwrap().to_string();

    for (sender, content) in [("claude", "cdraft"), ("codex", "xdraft")] {
        call_tool(
            app,
            "collab_send",
            json!({
                "session_id": session_id,
                "sender": sender,
                "topic": "draft",
                "content": content
            }),
        );
    }
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "canonical plan v1"
        }),
    );
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string()
        }),
    );
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": final_plan }).to_string()
        }),
    );
    let status = call_tool(app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanLocked");
    session_id
}

fn plan_hash(app: &App, session_id: &str) -> String {
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    status["final_plan_hash"].as_str().unwrap().to_string()
}

fn task_list_payload(plan_hash: &str, base_sha: &str, head_sha: &str, n: usize) -> String {
    let tasks: Vec<_> = (1..=n)
        .map(|i| {
            json!({
                "id": i,
                "title": format!("task {i}"),
                "acceptance": [format!("criterion {i}")]
            })
        })
        .collect();
    json!({
        "plan_hash": plan_hash,
        "base_sha": base_sha,
        "head_sha": head_sha,
        "tasks": tasks,
    })
    .to_string()
}

/// Send `implementation_done` from Claude, advancing the batch phase to
/// local review (`CodeReviewLocalPending`).
fn do_implementation_done(app: &App, session_id: &str, head: &str) {
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": head }).to_string()
        }),
    );
}

#[test]
fn collab_v2_happy_path_reaches_coding_complete() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "final plan text");
    let hash = plan_hash(&app, &session_id);

    // Submit a 2-task list — server stores the manifest for audit but
    // does not iterate it; Claude orchestrates subagents on its side.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "base0", "head0", 2)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["tasks_count"], 2);
    assert_eq!(status["base_sha"], "base0");

    // Single batch send replaces the per-task loop.
    do_implementation_done(&app, &session_id, "batch_head");
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["last_head_sha"], "batch_head");

    // Local → global review_fix → final_review (v3 linear, terminal in 3 turns).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": "h2" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": "h2" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final_review",
            "content": json!({ "head_sha": "h2", "pr_url": "https://example/pr/1" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodingComplete");
    assert_eq!(status["pr_url"], "https://example/pr/1");
    assert_eq!(status["last_head_sha"], "h2");

    // CodingComplete is a terminal phase — collab_end must be accepted.
    let ended = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert_eq!(ended["ok"], true);
}

#[test]
fn collab_v3_implementation_done_jumps_to_local_review() {
    // v3 batch mode: a single `implementation_done` send transitions
    // `CodeImplementPending` → `CodeReviewLocalPending` with Claude as owner.
    // No per-task review/fix turns server-side.
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 3)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_owner"], "claude");

    do_implementation_done(&app, &session_id, "batch_head");
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], "batch_head");
}

#[test]
fn collab_v3_unknown_per_task_topics_rejected() {
    // The old per-task topics (`implement`, `review_fix`) are no longer
    // accepted. They must surface a clear "unknown collab topic" error
    // rather than be silently dispatched.
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 1)
        }),
    );

    for (sender, topic) in [("claude", "implement"), ("codex", "review_fix")] {
        let err = call_tool_expect_error(
            &app,
            "collab_send",
            json!({
                "session_id": session_id,
                "sender": sender,
                "topic": topic,
                "content": json!({ "head_sha": "h1" }).to_string()
            }),
        );
        assert!(
            err.contains("unknown collab topic"),
            "expected unknown-topic error for {topic}, got: {err}"
        );
    }
}

#[test]
fn collab_start_accepts_implementer_codex_and_routes_owner() {
    // `--implementer=codex` flips the owner of `CodeImplementPending` to
    // Codex. Claude still publishes `task_list`; Codex is the only valid
    // sender of `implementation_done`.
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked_with_implementer(&app, "fp", Some("codex"));

    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["implementer"], "codex");

    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 2)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_owner"], "codex");

    // Claude trying to fire `implementation_done` is rejected.
    let err = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "batch_head" }).to_string()
        }),
    );
    assert!(
        err.to_lowercase().contains("not your turn") || err.contains("expects sender"),
        "expected turn-ownership error, got: {err}"
    );

    // Codex fires it and the phase advances to local review (Claude-owned).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "batch_head" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "claude");
}

#[test]
fn collab_start_rejects_invalid_implementer() {
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "implementer": "gemini"
        }),
    );
    assert!(
        err.to_lowercase().contains("agent")
            || err.to_lowercase().contains("must be 'claude' or 'codex'"),
        "expected agent-validation error, got: {err}"
    );
}

#[test]
fn collab_v2_end_rejected_in_coding_active_phase() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 1)
        }),
    );
    // Now in CodeImplementPending — collab_end must be rejected.
    let blocked = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert!(blocked["error"]
        .as_str()
        .unwrap_or("")
        .contains("active phase CodeImplementPending"));

    // Session still active — `implementation_done` should advance it.
    let ok = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "h1" }).to_string()
        }),
    );
    assert_eq!(ok["phase"], "CodeReviewLocalPending");
}

#[test]
fn collab_v2_wait_my_turn_dynamic_terminal_set() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");

    // Pre-task_list: PlanLocked is terminal. wait_my_turn returns immediately
    // with is_my_turn=false and phase=PlanLocked for either agent — the
    // terminal check fires before the ownership check.
    let wait = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": "codex", "timeout_secs": 1 }),
    );
    assert_eq!(wait["phase"], "PlanLocked");
    assert_eq!(wait["is_my_turn"], false);

    // Submit task_list → terminal set flips to {CodingComplete, CodingFailed}.
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 1)
        }),
    );
    // CodeImplementPending is NOT terminal — wait for claude returns is_my_turn=true.
    let wait = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": "claude", "timeout_secs": 1 }),
    );
    assert_eq!(wait["phase"], "CodeImplementPending");
    assert_eq!(wait["is_my_turn"], true);
}

#[test]
fn collab_v2_failure_report_transitions_to_coding_failed() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 1)
        }),
    );
    // Codex detects drift and emits failure_report.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "failure_report",
            "content": json!({ "coding_failure": "branch_drift: expected=b0 got=b1" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodingFailed");
    assert!(status["coding_failure"]
        .as_str()
        .unwrap_or("")
        .contains("branch_drift"));

    // Terminal: collab_end now succeeds (no longer coding-active).
    let ended = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert_eq!(ended["ok"], true);
}

#[test]
fn collab_start_code_review_happy_path_reaches_coding_complete() {
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, descendant_sha, _drift_sha) = git_repo_fixture();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    let wait = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": "codex", "timeout_secs": 1 }),
    );
    assert_eq!(wait["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(wait["is_my_turn"], true);

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["last_head_sha"], descendant_sha);

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final_review",
            "content": json!({ "head_sha": descendant_sha, "pr_url": "https://example/pr/42" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodingComplete");
    assert_eq!(status["pr_url"], "https://example/pr/42");
}

#[test]
fn collab_start_code_review_end_rejected_during_active_review() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/review-shortcut",
            "base_sha": "base0",
            "head_sha": "head0",
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    let blocked = call_tool_expect_error(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert!(blocked.contains("active phase CodeReviewFixGlobalPending"));
}

#[test]
fn collab_start_code_review_failure_report_reaches_coding_failed() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/review-shortcut",
            "base_sha": "base0",
            "head_sha": "head0",
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "failure_report",
            "content": json!({ "coding_failure": "branch_drift: expected=head0 got=headX" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodingFailed");
    assert!(status["coding_failure"]
        .as_str()
        .unwrap_or("")
        .contains("branch_drift"));
}

#[test]
fn collab_start_code_review_accepts_descendant_head_and_rejects_end_in_final_review() {
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, descendant_sha, _drift_sha) = git_repo_fixture();

    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], descendant_sha);

    let blocked = call_tool_expect_error(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert!(blocked.contains("active phase CodeReviewFinalPending"));
}

#[test]
fn collab_start_code_review_rejects_non_descendant_head() {
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, _descendant_sha, drift_sha) = git_repo_fixture();

    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    let blocked = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": drift_sha }).to_string()
        }),
    );
    assert!(blocked.contains("branch_drift"));
    assert!(blocked.contains("last_head_sha"));
}

#[test]
fn collab_start_code_review_operational_git_failure_is_not_branch_drift() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/definitely/not/a/repo",
            "branch": "feat/review-shortcut",
            "base_sha": "abc123",
            "head_sha": "def456",
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    let blocked = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": "def457" }).to_string()
        }),
    );
    assert!(blocked.contains("git ancestry validation failed"));
    assert!(!blocked.contains("branch_drift"));
}

#[test]
fn collab_v2_task_list_rejects_wrong_plan_hash() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");

    let bad = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload("deadbeef", "b0", "h0", 1)
        }),
    );
    assert!(bad["error"]
        .as_str()
        .unwrap_or("")
        .contains("plan_hash mismatch"));
}

#[test]
fn collab_v2_task_list_rejects_empty_acceptance() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");
    let hash = plan_hash(&app, &session_id);
    let bad_payload = json!({
        "plan_hash": hash,
        "base_sha": "b0",
        "head_sha": "h0",
        "tasks": [ { "id": 1, "title": "t", "acceptance": [] } ],
    })
    .to_string();
    let bad = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": bad_payload
        }),
    );
    assert!(bad["error"]
        .as_str()
        .unwrap_or("")
        .contains("acceptance criterion"));
}

// ── collab_recv auto_ack tests ────────────────────────────────────────────────

/// Helper: start a fresh session and send `count` messages from claude to
/// codex, returning (session_id, vec_of_message_ids).
fn setup_session_with_messages(app: &App, count: usize) -> (String, Vec<String>) {
    let started = call_tool(
        app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude" }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();

    // Submit claude's draft so the session has pending messages for codex.
    let mut ids = Vec::new();
    // We send the first message via collab_send (which also advances state);
    // then send bare messages directly via the low-level `collab_send` topic
    // "draft" for the first one.  For a simpler setup we drive through
    // PlanParallelDrafts and then do extra sends.
    //
    // Simpler approach: just send drafts from both sides so the parallel-draft
    // phase finishes and messages are visible, then query pending messages via
    // collab_recv. But we want deterministic IDs. Instead we'll use the fact
    // that after both drafts are submitted the session moves to
    // PlanSynthesisPending where codex has a pending draft message. We can
    // also query the recv output to capture the IDs.
    //
    // For simplicity: submit both drafts, then read back the IDs from the
    // first recv (without auto_ack) so we have them for assertions.
    assert!(count >= 1, "need at least one message for setup");

    call_tool(
        app,
        "collab_send",
        json!({ "session_id": session_id, "sender": "claude", "topic": "draft", "content": "cdraft" }),
    );
    // After claude's draft the phase is still PlanParallelDrafts; codex has
    // no pending messages yet (parallel drafts are blind until codex submits).
    // Submit codex's draft to unblock visibility.
    call_tool(
        app,
        "collab_send",
        json!({ "session_id": session_id, "sender": "codex", "topic": "draft", "content": "xdraft" }),
    );
    // Now codex can see claude's draft. Collect it.
    let recv = call_tool(
        app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    for msg in recv["messages"].as_array().unwrap() {
        ids.push(msg["id"].as_str().unwrap().to_string());
    }
    (session_id, ids)
}

#[test]
fn recv_auto_ack_true_marks_messages_acked() {
    let app = App::open_for_test().unwrap();
    let (session_id, first_ids) = setup_session_with_messages(&app, 1);
    assert!(
        !first_ids.is_empty(),
        "setup must produce at least one message"
    );

    // First recv without auto_ack to confirm message is visible.
    let recv1 = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    assert!(
        !recv1["messages"].as_array().unwrap().is_empty(),
        "message must be visible before auto_ack"
    );

    // Recv with auto_ack=true — this atomically marks the messages as acked.
    let recv2 = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50, "auto_ack": true }),
    );
    assert!(
        !recv2["messages"].as_array().unwrap().is_empty(),
        "auto_ack recv must still return the messages"
    );

    // Subsequent recv must return nothing — all messages are now acked.
    let recv3 = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    assert!(
        recv3["messages"].as_array().unwrap().is_empty(),
        "no pending messages should remain after auto_ack recv"
    );
}

#[test]
fn recv_auto_ack_false_default_does_not_ack() {
    let app = App::open_for_test().unwrap();
    let (session_id, _) = setup_session_with_messages(&app, 1);

    // Recv without auto_ack (default false).
    let recv1 = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    let count1 = recv1["messages"].as_array().unwrap().len();
    assert!(count1 > 0, "must have pending messages");

    // Second recv without auto_ack must return the same messages (still pending).
    let recv2 = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    assert_eq!(
        recv2["messages"].as_array().unwrap().len(),
        count1,
        "messages must still be pending after non-auto_ack recv"
    );
}

#[test]
fn recv_auto_ack_with_limit_only_acks_returned_messages() {
    // Send 5 draft messages for codex by driving through multiple canonical
    // sends. For this test we just need multiple unacked messages visible to
    // codex after both parallel drafts are submitted. We'll accumulate
    // messages by acking selectively.
    //
    // Simpler: after both drafts are submitted, codex has 1 message (claude's
    // draft). We drive forward to get more: claude sends canonical → codex
    // reviews → claude sends final → codex has more messages queued.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude" }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();

    // Both drafts → PlanSynthesisPending; now codex has 1 pending message.
    call_tool(
        &app,
        "collab_send",
        json!({ "session_id": session_id, "sender": "claude", "topic": "draft", "content": "cdraft" }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({ "session_id": session_id, "sender": "codex", "topic": "draft", "content": "xdraft" }),
    );

    // Claude sends canonical → codex now has 2 pending messages (draft + canonical).
    call_tool(
        &app,
        "collab_send",
        json!({ "session_id": session_id, "sender": "claude", "topic": "canonical", "content": "canonical plan" }),
    );

    // Verify 2 pending messages for codex.
    let all_recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    let total = all_recv["messages"].as_array().unwrap().len();
    assert!(
        total >= 2,
        "need at least 2 messages for this test, got {total}"
    );

    // Recv with limit=1 and auto_ack=true: only 1 message returned and acked.
    let limited_recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 1, "auto_ack": true }),
    );
    assert_eq!(
        limited_recv["messages"].as_array().unwrap().len(),
        1,
        "limited recv must return exactly 1 message"
    );

    // The remaining messages (total - 1) must still be pending.
    let remaining_recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "limit": 50 }),
    );
    assert_eq!(
        remaining_recv["messages"].as_array().unwrap().len(),
        total - 1,
        "only the returned message should have been acked; others must remain pending"
    );
}
