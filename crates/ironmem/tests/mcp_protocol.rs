//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests call `dispatch` directly with an in-memory App (noop embedder,
//! no ONNX model required) and assert on the JSON-RPC response shape.

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::json;

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
    let started = call_tool(
        app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude"
        }),
    );
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

/// Run implement → review → verdict=agree, advancing one task.
fn happy_task_cycle(app: &App, session_id: &str, head: &str) {
    for (sender, topic) in [("claude", "implement"), ("codex", "review")] {
        call_tool(
            app,
            "collab_send",
            json!({
                "session_id": session_id,
                "sender": sender,
                "topic": topic,
                "content": json!({ "head_sha": head }).to_string()
            }),
        );
    }
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "verdict",
            "content": json!({ "head_sha": head, "verdict": "agree" }).to_string()
        }),
    );
}

#[test]
fn collab_v2_happy_path_reaches_coding_complete() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "final plan text");
    let hash = plan_hash(&app, &session_id);

    // Submit a 2-task list.
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
    assert_eq!(status["current_task_index"], 0);
    assert_eq!(status["base_sha"], "base0");

    // Task 1 + Task 2 happy path.
    happy_task_cycle(&app, &session_id, "h1");
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_task_index"], 1);
    happy_task_cycle(&app, &session_id, "h2");
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");

    // Local review → global review agree → PR ready.
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
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_global",
            "content": json!({ "head_sha": "h2", "verdict": "agree" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PrReadyPending");

    // PR opened → CodingComplete.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "pr_opened",
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
fn collab_v2_task_disagree_round_loops_back_to_review() {
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

    // implement → review → verdict=disagree → debate comment → final
    // (under cap, so Final loops back to CodeReviewPending).
    for (sender, topic, extra) in [
        ("claude", "implement", json!({ "head_sha": "h1" })),
        ("codex", "review", json!({ "head_sha": "h1" })),
        (
            "claude",
            "verdict",
            json!({ "head_sha": "h1", "verdict": "disagree_with_reasons" }),
        ),
        ("codex", "comment", json!({ "head_sha": "h1" })),
        ("claude", "final", json!({ "head_sha": "h1" })),
    ] {
        call_tool(
            &app,
            "collab_send",
            json!({
                "session_id": session_id,
                "sender": sender,
                "topic": topic,
                "content": extra.to_string()
            }),
        );
    }
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewPending");
    assert_eq!(status["task_review_round"], 1);
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

    // Session still active — send should work.
    let ok = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implement",
            "content": json!({ "head_sha": "h1" }).to_string()
        }),
    );
    assert_eq!(ok["phase"], "CodeReviewPending");
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
