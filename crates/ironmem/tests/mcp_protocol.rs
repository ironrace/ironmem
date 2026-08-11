//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests call `dispatch` directly with an in-memory App (noop embedder,
//! no ONNX model required) and assert on the JSON-RPC response shape.

use ironmem::collab::Agent;
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

static COMPACT_RESPONSES_ENV_LOCK: Mutex<()> = Mutex::new(());

struct CompactResponsesEnvGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl CompactResponsesEnvGuard {
    fn enabled() -> Self {
        Self::set(Some("1"))
    }

    fn disabled() -> Self {
        Self::set(None)
    }

    fn set(value: Option<&str>) -> Self {
        let lock = COMPACT_RESPONSES_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("IRONMEM_COMPACT_RESPONSES");
        match value {
            Some(value) => std::env::set_var("IRONMEM_COMPACT_RESPONSES", value),
            None => std::env::remove_var("IRONMEM_COMPACT_RESPONSES"),
        }
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for CompactResponsesEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("IRONMEM_COMPACT_RESPONSES", value),
            None => std::env::remove_var("IRONMEM_COMPACT_RESPONSES"),
        }
    }
}

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

fn sha256_hex(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

fn add_large_search_drawers(
    app: &App,
    query: &str,
    wing: &str,
    room: &str,
    count: usize,
) -> Vec<String> {
    (0..count)
        .map(|index| {
            let content = format!(
                "{query} large search fixture {index} {}",
                "large-content ".repeat(600)
            );
            let added = call_tool(
                app,
                "add_drawer",
                json!({ "content": content, "wing": wing, "room": room }),
            );
            added["id"]
                .as_str()
                .expect("large search fixture returns an id")
                .to_owned()
        })
        .collect()
}

fn assert_search_reference_fields(hit: &serde_json::Value, wing: &str, room: &str) {
    assert!(
        hit["id"].as_str().is_some_and(|id| !id.is_empty()),
        "search hit must retain a stable id: {hit}"
    );
    assert_eq!(hit["wing"].as_str(), Some(wing));
    assert_eq!(hit["room"].as_str(), Some(room));
    assert!(
        hit["score"].is_number(),
        "search hit must retain score: {hit}"
    );
    assert!(
        hit["date"].is_string(),
        "search hit must retain date: {hit}"
    );
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
        "collab_set_implementer",
        "collab_set_pilot",
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

    for blocked in &[
        "add_drawer",
        "delete_drawer",
        "diary_write",
        "collab_set_implementer",
        "collab_set_pilot",
    ] {
        assert!(
            !names.contains(blocked),
            "write tool should be absent in read-only mode: {blocked}"
        );
    }
    // Read tools still present
    assert!(names.contains(&"status"));
    assert!(names.contains(&"search"));
    // get_drawer is a read tool — it must remain available in read-only mode.
    assert!(names.contains(&"get_drawer"));
}

#[test]
fn get_drawer_round_trips_by_id() {
    let app = App::open_for_test().unwrap();

    // A body larger than the search-excerpt cap (MAX_SENSITIVE_FIELD_CHARS =
    // 4_000): verifies by-id fetch returns the FULL stored body, the case that
    // broke the collab compose→submit handoff when only semantic search existed.
    let body = "y".repeat(4_500);
    let added = call_tool(
        &app,
        "add_drawer",
        json!({"content": body, "wing": "ironrace-memory", "room": "collab-drafts"}),
    );
    let id = added["id"].as_str().expect("add_drawer returns id");

    let got = call_tool(&app, "get_drawer", json!({ "id": id }));
    assert_eq!(got["found"].as_bool(), Some(true));
    assert_eq!(got["id"].as_str(), Some(id));
    assert_eq!(
        got["content"].as_str(),
        Some(body.as_str()),
        "full body must round-trip verbatim, not a truncated excerpt"
    );
    assert_eq!(got["content_truncated"].as_bool(), Some(false));

    // A well-formed but absent id reports found:false (not an error).
    let missing = call_tool(&app, "get_drawer", json!({ "id": "0".repeat(32) }));
    assert_eq!(missing["found"].as_bool(), Some(false));
}

#[test]
fn search_defaults_to_excerpt_and_get_drawer_dereferences_the_full_body() {
    // `IRONMEM_COMPACT_RESPONSES` is process-global and flipped by the
    // `search_response_*` tests in this same binary. Every assertion below
    // reads `results` as a plain array, which the compact envelope replaces
    // with a `__compact_v1` object — so this test has to hold the same lock
    // those tests use, not merely assume the variable is unset.
    let _compact_guard = CompactResponsesEnvGuard::disabled();
    let app = App::open_for_test().unwrap();
    let body = format!(
        "MCP protocol fixture prefix. {} needle exact body. {}after",
        "before ".repeat(80),
        "after ".repeat(79)
    );
    let added = call_tool(
        &app,
        "add_drawer",
        json!({ "content": body, "wing": "protocol-tests", "room": "mcp" }),
    );
    let id = added["id"]
        .as_str()
        .expect("add_drawer returns a stable id")
        .to_owned();

    let search = call_tool(&app, "search", json!({ "query": "needle", "limit": 1 }));
    assert_eq!(search["content_mode"], "excerpt");
    let hit = search["results"]
        .as_array()
        .and_then(|results| results.iter().find(|result| result["id"] == id))
        .expect("search should return the inserted drawer");
    assert_eq!(hit["id"], id);
    let excerpt = hit["excerpt"]
        .as_str()
        .expect("search hit returns an excerpt");
    assert!(!excerpt.is_empty());
    assert!(excerpt.contains("needle"));
    assert!(excerpt.chars().count() < body.chars().count());
    assert!(excerpt.chars().count() <= 300);
    assert!(hit.get("content").is_none());

    let drawer = call_tool(&app, "get_drawer", json!({ "id": id }));
    assert_eq!(drawer["found"], true);
    assert_eq!(
        drawer["content"].as_str(),
        Some(body.as_str()),
        "get_drawer must return the verbatim body behind the search hit"
    );
}

#[test]
fn search_full_returns_content_over_mcp() {
    // `IRONMEM_COMPACT_RESPONSES` is process-global and flipped by the
    // `search_response_*` tests in this same binary. Every assertion below
    // reads `results` as a plain array, which the compact envelope replaces
    // with a `__compact_v1` object — so this test has to hold the same lock
    // those tests use, not merely assume the variable is unset.
    let _compact_guard = CompactResponsesEnvGuard::disabled();
    let app = App::open_for_test().unwrap();
    let body = "MCP full search fixture with the exact body";
    let added = call_tool(
        &app,
        "add_drawer",
        json!({ "content": body, "wing": "protocol-tests", "room": "mcp" }),
    );
    let id = added["id"]
        .as_str()
        .expect("add_drawer returns a stable id");

    let search = call_tool(
        &app,
        "search",
        json!({ "query": "full search fixture", "full": true, "limit": 1 }),
    );
    assert_eq!(search["content_mode"], "full");
    let hit = search["results"]
        .as_array()
        .and_then(|results| results.iter().find(|result| result["id"] == id))
        .expect("full search should return the inserted drawer");
    assert_eq!(hit["content"], body);
    assert!(hit.get("excerpt").is_none());
}

#[test]
fn search_response_compacted_when_enabled() {
    let app = App::open_for_test().unwrap();
    let query = "compact-search-response-fixture";

    for index in 0..3 {
        call_tool(
            &app,
            "add_drawer",
            json!({
                "content": format!("{query} result {index}"),
                "wing": "protocol-tests",
                "room": "mcp",
            }),
        );
    }

    let original_results = {
        let _guard = CompactResponsesEnvGuard::disabled();
        call_tool(&app, "search", json!({ "query": query, "limit": 3 }))["results"].clone()
    };
    let response = {
        let _guard = CompactResponsesEnvGuard::enabled();
        call_tool(&app, "search", json!({ "query": query, "limit": 3 }))
    };
    let compacted_results = &response["results"];
    assert!(
        compacted_results.get("__compact_v1").is_some(),
        "enabled search result must carry the compact envelope: {response}"
    );
    let expanded = ironmem::mcp::compact::expand_compact_value(compacted_results);
    assert_eq!(expanded, original_results);
}

#[test]
fn search_response_unchanged_when_disabled() {
    let _guard = CompactResponsesEnvGuard::disabled();
    let app = App::open_for_test().unwrap();
    let query = "uncompacted-search-response-fixture";

    for index in 0..3 {
        call_tool(
            &app,
            "add_drawer",
            json!({
                "content": format!("{query} result {index}"),
                "wing": "protocol-tests",
                "room": "mcp",
            }),
        );
    }

    let response = call_tool(&app, "search", json!({ "query": query, "limit": 3 }));
    assert!(response["results"].is_array());
    assert!(response["results"].get("__compact_v1").is_none());
}

#[test]
fn search_rejects_string_full_over_mcp() {
    let app = App::open_for_test().unwrap();
    let error = call_tool_expect_error(&app, "search", json!({ "query": "x", "full": "true" }));
    assert_eq!(error, "full must be a boolean");
}

#[test]
fn search_response_budget_preserves_references_in_excerpt_and_full_modes() {
    // `IRONMEM_COMPACT_RESPONSES` is process-global and flipped by the
    // `search_response_*` tests in this same binary. Every assertion below
    // reads `results` as a plain array, which the compact envelope replaces
    // with a `__compact_v1` object — so this test has to hold the same lock
    // those tests use, not merely assume the variable is unset.
    let _compact_guard = CompactResponsesEnvGuard::disabled();
    let app = App::open_for_test().unwrap();
    let query = "aggregatebudgetfixture";
    let wing = "protocol-tests";
    let room = "mcp";
    let ids = add_large_search_drawers(&app, query, wing, room, 25);

    let excerpt_response = call_tool(&app, "search", json!({ "query": query, "limit": 25 }));
    let excerpt_results = excerpt_response["results"]
        .as_array()
        .expect("excerpt search results must be an array");
    assert_eq!(excerpt_results.len(), ids.len());
    assert_eq!(excerpt_response["content_mode"], "excerpt");
    let inserted_ids: HashSet<String> = ids.iter().cloned().collect();
    let mut excerpt_ids = HashSet::with_capacity(excerpt_results.len());
    // MAX_SEARCH_EXCERPT_CHARS * MAX_SEARCH_LIMIT is 300 * 25 = 7,500,
    // below MAX_SEARCH_RESPONSE_CHARS (32,000). Therefore excerpt mode cannot
    // exhaust the aggregate content budget; every bounded-page hit must retain
    // a usable excerpt and its dereference metadata.
    for hit in excerpt_results {
        assert_search_reference_fields(hit, wing, room);
        let id = hit["id"].as_str().expect("excerpt hit must have an id");
        assert!(
            excerpt_ids.insert(id.to_owned()),
            "excerpt results must not repeat an id: {hit}"
        );
        assert!(
            inserted_ids.contains(id),
            "excerpt hit id must identify an inserted drawer: {hit}"
        );
        let excerpt = hit["excerpt"]
            .as_str()
            .expect("excerpt mode must return an excerpt string");
        assert!(
            !excerpt.is_empty(),
            "excerpt mode must retain non-empty excerpts: {hit}"
        );
        assert!(
            excerpt.chars().count() <= 300,
            "excerpt mode must respect the 300-character cap: {hit}"
        );
        assert_eq!(hit["excerpt_truncated"], true);
        assert!(hit.get("content").is_none());
    }
    assert_eq!(excerpt_ids, inserted_ids);

    let full_response = call_tool(
        &app,
        "search",
        json!({ "query": query, "full": true, "limit": 25 }),
    );
    let full_results = full_response["results"]
        .as_array()
        .expect("full search results must be an array");
    assert_eq!(full_results.len(), ids.len());
    assert_eq!(full_response["content_mode"], "full");
    let mut full_ids = HashSet::with_capacity(full_results.len());
    // Each fixture body is larger than the 4,000-character per-field cap, so
    // the first eight results consume the 32,000-character aggregate budget.
    // Later results must remain page references even though their content is
    // empty, and call_tool has already parsed the tool text as valid JSON.
    for (index, hit) in full_results.iter().enumerate() {
        assert_search_reference_fields(hit, wing, room);
        let id = hit["id"].as_str().expect("full hit must have an id");
        assert!(
            full_ids.insert(id.to_owned()),
            "full results must not repeat an id: {hit}"
        );
        assert!(
            inserted_ids.contains(id),
            "full hit id must identify an inserted drawer: {hit}"
        );
        assert_eq!(hit["content_truncated"], true);
        let content = hit["content"]
            .as_str()
            .expect("full hit content must remain a string");
        if index < 8 {
            assert_eq!(content.chars().count(), 4_000);
        } else {
            assert!(content.is_empty(), "budget-exhausted content must be empty");
            let drawer = call_tool(&app, "get_drawer", json!({ "id": id }));
            assert_eq!(drawer["found"], true);
            assert_eq!(drawer["id"], id);
        }
        assert!(hit.get("excerpt").is_none());
    }
    assert_eq!(full_ids, inserted_ids);
}

#[test]
fn search_default_response_is_at_least_five_times_smaller_than_full_response() {
    // `IRONMEM_COMPACT_RESPONSES` is process-global and flipped by the
    // `search_response_*` tests in this same binary. Every assertion below
    // reads `results` as a plain array, which the compact envelope replaces
    // with a `__compact_v1` object — so this test has to hold the same lock
    // those tests use, not merely assume the variable is unset.
    let _compact_guard = CompactResponsesEnvGuard::disabled();
    let app = App::open_for_test().unwrap();
    let query = "sizefloorfixture";
    let ids = add_large_search_drawers(&app, query, "protocol-tests", "mcp", 10);

    let default_response = call_tool(&app, "search", json!({ "query": query, "limit": 10 }));
    let full_response = call_tool(
        &app,
        "search",
        json!({ "query": query, "full": true, "limit": 10 }),
    );
    assert_eq!(
        default_response["results"].as_array().map(Vec::len),
        Some(ids.len())
    );
    assert_eq!(
        full_response["results"].as_array().map(Vec::len),
        Some(ids.len())
    );

    let default_size = serde_json::to_vec(&default_response)
        .expect("default search response must serialize")
        .len();
    let full_size = serde_json::to_vec(&full_response)
        .expect("full search response must serialize")
        .len();
    assert!(
        full_size >= default_size.saturating_mul(5),
        "default response should be at least 5x smaller: default={default_size}, full={full_size}"
    );
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

    let reviews = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "claude" }),
    );
    let review = reviews["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["topic"] == "review")
        .expect("collab_approve must queue a review message for Claude");
    assert!(review.get("content").is_none());
    let review_drawer_id = review["drawer_id"]
        .as_str()
        .expect("approve review must have a durable drawer ref");
    let review_drawer = call_tool(&app, "get_drawer", json!({ "id": review_drawer_id }));
    assert_eq!(
        review_drawer["content"],
        json!({ "verdict": "approve", "content_hash": canonical_hash }).to_string()
    );

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
fn collab_request_changes_advances_to_finalize_and_locks() {
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

    // One-pass review (MAX_REVIEW_ROUNDS = 1): Codex requests changes, which no
    // longer returns to synthesis — it advances directly to
    // PlanClaudeFinalizePending so Claude can fold the requested changes into the
    // final plan and lock. review_round bumps to its single-pass cap of 1.
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
    assert_eq!(status_after_rc["phase"], "PlanClaudeFinalizePending");
    assert_eq!(status_after_rc["current_owner"], "claude");
    assert_eq!(status_after_rc["review_round"], 1);

    // Claude folds Codex's requested changes into the final plan (distinct from
    // the canonical body) and publishes it; the session locks.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": "Final plan: canonical v1 + Codex's changes" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanLocked");
    // The final plan differs from the single canonical body, so their hashes and
    // drawer refs must differ (no second canonical round re-stamped them equal).
    assert_ne!(status["final_plan_hash"], status["canonical_plan_hash"]);
    // Plan-by-reference (#90): by default collab_status now returns only the
    // compact references, so the full bodies are absent.
    assert!(status.get("canonical_plan").is_none());
    assert!(status.get("final_plan").is_none());
    let canonical_ref = status["canonical_plan_ref"]["drawer_id"].as_str().unwrap();
    let final_ref = status["final_plan_ref"]["drawer_id"].as_str().unwrap();
    assert_eq!(canonical_ref.len(), 32);
    assert_eq!(final_ref.len(), 32);
    assert_ne!(canonical_ref, final_ref);
    // A fresh agent joining at PlanLocked deliberately dereferences each plan
    // drawer. `verbose:true` remains accepted but must not reintroduce inline
    // plan text. The final drawer stores the PARSED plan text rather than the
    // {"plan":...} transport wrapper.
    let verbose = call_tool(
        &app,
        "collab_status",
        json!({ "session_id": session_id, "verbose": true }),
    );
    assert!(verbose.get("canonical_plan").is_none());
    assert!(verbose.get("final_plan").is_none());
    assert_eq!(verbose["canonical_plan_ref"]["drawer_id"], canonical_ref);
    assert_eq!(verbose["final_plan_ref"]["drawer_id"], final_ref);

    let canonical_drawer = call_tool(&app, "get_drawer", json!({ "id": canonical_ref }));
    let final_drawer = call_tool(&app, "get_drawer", json!({ "id": final_ref }));
    assert_eq!(canonical_drawer["content"], "Merged canonical v1");
    assert_eq!(
        final_drawer["content"],
        "Final plan: canonical v1 + Codex's changes"
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
        status.get("canonical_plan_ref").is_none(),
        "canonical_plan_ref must be absent before any canonical is published"
    );
    assert!(
        status.get("final_plan").is_none(),
        "final_plan must be absent before PlanLocked"
    );
    assert!(
        status.get("final_plan_ref").is_none(),
        "final_plan_ref must be absent before PlanLocked"
    );
}

#[test]
fn collab_single_review_caps_round_and_rejects_canonical_resend() {
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

    // Canonical v1, then the single allowed review. One-pass review
    // (MAX_REVIEW_ROUNDS = 1): request_changes caps review_round at 1 and
    // advances to PlanClaudeFinalizePending — there is no second review round.
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

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "PlanClaudeFinalizePending");
    assert_eq!(status["review_round"], 1);

    // Planning never loops back to synthesis, so a second canonical is rejected:
    // the phase now expects `final`, not another canonical round.
    let resend_err = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "v2"
        }),
    );
    assert!(
        resend_err.contains("PublishFinal"),
        "canonical re-send must be rejected with a finalize-phase mismatch, got: {resend_err}"
    );

    // Claude publishes final despite Codex's objection; the round stays capped at
    // 1 and the session locks.
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
    assert_eq!(status["review_round"], 1);
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
    // Pin of the `pilot`-omitted default: `pilot` resolves to `claude` and
    // `current_owner` seeds at `copilot(claude)` = `codex`, exactly today's
    // pre-Task-8 behavior. If this ever drifts, it must be a deliberate
    // change to the resolved default, not a silent regression.
    assert_eq!(started["pilot"], "claude");
    let session_id = started["session_id"].as_str().unwrap();

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["task"], "review landing page branch");
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["pilot"], "claude");
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
fn collab_start_code_review_rejects_codex_initiator_even_with_pilot_codex() {
    // The dispatcher-side `initiator must be 'claude'` check is orthogonal to
    // `pilot`: it must reject a non-claude initiator unconditionally, even
    // when the caller also asks for `pilot=codex`.
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
            "pilot": "codex",
            "task": "review landing page branch"
        }),
    );
    assert!(error.contains("initiator must be 'claude'"));
}

#[test]
fn collab_start_code_review_pilot_codex_mirrors_owners_and_refuses_off_role() {
    // `pilot=codex` review-only session: `start_global_review_session` seeds
    // `current_owner = copilot(codex)` = `claude` (the copilot always moves
    // first in the v3 global-review linear flow). Read from `apply_event`'s
    // `CodeReviewFixGlobalPending`/`CodeReviewLocalPending`/
    // `CodeReviewFinalPending` arms (crates/ironmem/src/collab/state_machine/mod.rs):
    //   CodeReviewFixGlobalPending, owner=claude (copilot)
    //     --review_fix_global(claude)--> CodeReviewLocalPending, owner=codex (pilot)
    //     --review_local(codex)--> CodeReviewFinalPending, owner=codex (pilot)
    //     --final_review(codex)--> CodingComplete, owner=codex (pilot)
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, descendant_sha, _drift_sha) = git_repo_fixture();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut-codex-pilot",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "pilot": "codex",
            "task": "review completed branch"
        }),
    );
    assert_eq!(started["pilot"], "codex");
    let session_id = started["session_id"].as_str().unwrap();

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["pilot"], "codex");

    // Off-role: codex is the pilot, not the current owner yet — refused.
    let err = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    assert!(
        err.to_lowercase().contains("not your turn"),
        "expected turn-ownership error, got: {err}"
    );

    // Claude (copilot) submits review_fix_global -> owner flips to codex (pilot).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "codex");

    // Off-role: claude is the copilot now, not the owner — refused.
    let err = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    assert!(
        err.to_lowercase().contains("not your turn"),
        "expected turn-ownership error, got: {err}"
    );

    // Codex (pilot) submits review_local -> stays owner into CodeReviewFinalPending.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_local",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "codex");

    // Off-role: claude tries the final PR turn — refused.
    let err = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final_review",
            "content": json!({ "head_sha": descendant_sha, "pr_url": "https://example/pr/9" }).to_string()
        }),
    );
    assert!(
        err.to_lowercase().contains("not your turn"),
        "expected turn-ownership error, got: {err}"
    );

    // Codex (pilot) submits final_review -> CodingComplete.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "final_review",
            "content": json!({ "head_sha": descendant_sha, "pr_url": "https://example/pr/9" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodingComplete");
    assert_eq!(status["pilot"], "codex");
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
        json!({ "session_id": session_id, "receiver": "codex", "auto_ack": true }),
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
    assert_eq!(messages[0]["first_200_chars"], "claude draft");
    assert!(
        messages[0].get("content").is_none(),
        "default collab_recv must not inline message content"
    );
}

#[test]
fn collab_recv_defaults_to_drawer_refs_that_get_drawer_can_dereference() {
    // `IRONMEM_COMPACT_RESPONSES` is process-global and flipped by the
    // `search_response_*` tests in this same binary. Every assertion below
    // reads `results` as a plain array, which the compact envelope replaces
    // with a `__compact_v1` object — so this test has to hold the same lock
    // those tests use, not merely assume the variable is unset.
    let _compact_guard = CompactResponsesEnvGuard::disabled();
    let app = App::open_for_test().unwrap();
    let content = "Claude's durable draft body";
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
            "content": content,
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex's separate draft",
        }),
    );

    let recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex" }),
    );
    let messages = recv["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    let message = &messages[0];

    // The established delivery envelope remains intact.
    assert!(message["id"].is_string());
    assert_eq!(message["sender"], "claude");
    assert_eq!(message["topic"], "draft");
    assert!(message["created_at"].is_string());

    // The default is compact and dereferenceable rather than body-inline.
    assert!(message.get("content").is_none());
    let drawer_id = message["drawer_id"]
        .as_str()
        .expect("new collab messages must carry a drawer id");
    assert_eq!(message["hash"], sha256_hex(content));
    assert_eq!(message["first_200_chars"], content);

    let drawer = call_tool(&app, "get_drawer", json!({ "id": drawer_id }));
    assert_eq!(drawer["found"], true);
    assert_eq!(drawer["content"], content);
    assert_eq!(drawer["wing"], "ironrace-memory");
    assert_eq!(drawer["room"], "collab-messages");

    assert_ne!(
        drawer_id,
        ironmem::db::drawers::generate_id(content, "ironrace-memory", "collab-messages"),
        "transport refs must be opaque rather than derivable from message content"
    );
    let search = call_tool(&app, "search", json!({ "query": content }));
    assert!(
        !search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|result| result["id"] == drawer_id),
        "transport drawers must not be discoverable through generic search"
    );
    let delete_error = call_tool_expect_error(&app, "delete_drawer", json!({ "id": drawer_id }));
    assert!(delete_error.contains("referenced by collab state"));
}

#[test]
fn collab_recv_full_true_retains_inline_content_and_drawer_refs() {
    let app = App::open_for_test().unwrap();
    let content = "Claude's full legacy-compatible draft";
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
            "content": content,
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex's separate draft",
        }),
    );

    let recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex", "full": true }),
    );
    let messages = recv["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    let message = &messages[0];
    assert_eq!(message["content"], content);
    assert!(message["drawer_id"].is_string());
    assert_eq!(message["hash"], sha256_hex(content));
    assert_eq!(message["first_200_chars"], content);
}

#[test]
fn collab_recv_preview_is_bounded_by_rust_characters_not_bytes() {
    let app = App::open_for_test().unwrap();
    let content = "🦀".repeat(201);
    let expected_preview: String = content.chars().take(200).collect();
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
            "content": content,
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex's separate draft",
        }),
    );

    let recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex" }),
    );
    let preview = recv["messages"][0]["first_200_chars"]
        .as_str()
        .expect("compact message ref must include a preview");
    assert_eq!(preview, expected_preview);
    assert_eq!(preview.chars().count(), 200);
    assert!(
        preview.len() > 200,
        "a 200-character Unicode preview must not be byte-truncated"
    );
    assert!(recv["messages"][0].get("content").is_none());
}

#[test]
fn collab_recv_legacy_null_drawer_id_inlines_content_by_default() {
    let app = App::open_for_test().unwrap();
    let content = "legacy inline message body";
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

    let sent = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": content,
        }),
    );
    let message_id = sent["message_id"].as_str().unwrap();
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex's separate draft",
        }),
    );

    // Simulate a pre-016 queue row whose migration had no drawer reference.
    app.db
        .with_transaction(|tx| {
            tx.execute(
                "UPDATE messages SET drawer_id = NULL WHERE id = ?1",
                [message_id],
            )?;
            Ok(())
        })
        .unwrap();

    let recv = call_tool(
        &app,
        "collab_recv",
        json!({ "session_id": session_id, "receiver": "codex" }),
    );
    let message = &recv["messages"][0];
    assert!(message["drawer_id"].is_null());
    assert_eq!(message["content"], content);
    assert_eq!(message["hash"], sha256_hex(content));
    assert_eq!(message["first_200_chars"], content);
}

#[test]
fn collab_recv_redacts_message_content_and_derivatives_in_restricted_mode() {
    use ironmem::config::McpAccessMode;

    // Restricted mode correctly disallows writes, so establish a real collab
    // message through the public trusted-mode protocol first, then switch the
    // same persisted fixture to its restricted read view.
    let mut app = App::open_for_test().unwrap();
    let content = "SENSITIVE: do not expose the launch password";
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();
    let sent = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": content,
        }),
    );
    let message_id = sent["message_id"].as_str().unwrap().to_string();
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex's separate draft",
        }),
    );
    app.config.mcp_access_mode = McpAccessMode::Restricted;

    for args in [
        json!({ "session_id": session_id, "receiver": "codex" }),
        json!({ "session_id": session_id, "receiver": "codex", "full": true }),
    ] {
        let recv = call_tool(&app, "collab_recv", args);
        let message = &recv["messages"][0];
        let encoded = message.to_string();

        assert_eq!(message["id"], message_id);
        assert_eq!(message["sender"], "claude");
        assert_eq!(message["topic"], "draft");
        assert_eq!(message["content_redacted"], true);
        assert!(message.get("hash").is_none());
        assert_eq!(message["hash_redacted"], true);
        assert!(message.get("drawer_id").is_none());
        assert!(message.get("first_200_chars").is_none());
        assert!(message.get("content").is_none());
        assert!(
            !encoded.contains(content)
                && !encoded.contains(&sha256_hex(content)),
            "restricted collab_recv must not expose a body or content-derived fingerprint: {encoded}"
        );
    }
}

#[test]
fn collab_recv_redacts_legacy_inline_content_in_restricted_mode() {
    use ironmem::config::McpAccessMode;

    let mut app = App::open_for_test().unwrap();
    let content = "SENSITIVE legacy message body";
    let started = call_tool(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude" }),
    );
    let session_id = started["session_id"].as_str().unwrap();
    let sent = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": content,
        }),
    );
    let message_id = sent["message_id"].as_str().unwrap();
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "Codex's separate draft",
        }),
    );
    app.db
        .with_transaction(|tx| {
            tx.execute(
                "UPDATE messages SET drawer_id = NULL WHERE id = ?1",
                [message_id],
            )?;
            Ok(())
        })
        .unwrap();
    app.config.mcp_access_mode = McpAccessMode::Restricted;

    for args in [
        json!({ "session_id": session_id, "receiver": "codex" }),
        json!({ "session_id": session_id, "receiver": "codex", "full": true }),
    ] {
        let message = &call_tool(&app, "collab_recv", args)["messages"][0];
        assert_eq!(message["content_redacted"], true);
        assert_eq!(message["hash_redacted"], true);
        for field in ["drawer_id", "hash", "first_200_chars", "content"] {
            assert!(
                message.get(field).is_none(),
                "restricted legacy receive must omit {field}"
            );
        }
        assert!(!message.to_string().contains(content));
    }
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
fn collab_wait_my_turn_returns_compact_frame_when_timeout_is_unsettled() {
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

    // The fresh session belongs to Claude, so Codex remains unsettled until
    // its minimum one-second timeout elapses.
    let resp = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": "codex", "timeout_secs": 1 }),
    );

    assert_eq!(resp, json!({"unchanged": true}));
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
fn collab_start_rejects_duplicate_active_session_on_same_branch() {
    let app = App::open_for_test().unwrap();
    let first = call_tool(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude", "task": "first" }),
    );
    let first_id = first["session_id"].as_str().unwrap().to_string();

    // A stray replay of `/collab start` on the same repo+branch (e.g. a fired
    // ScheduleWakeup re-running the entry command) must be rejected, not
    // silently fork a second session.
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude", "task": "second" }),
    );
    assert!(
        err.contains("active collab session already exists"),
        "expected duplicate-session rejection, got: {err}"
    );
    assert!(
        err.contains(&first_id),
        "error must name the existing session id so the user can resume it, got: {err}"
    );
}

#[test]
fn collab_start_allows_distinct_branch_in_same_process() {
    let app = App::open_for_test().unwrap();
    let first = call_tool(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude" }),
    );
    let first_id = first["session_id"].as_str().unwrap().to_string();

    let second = call_tool(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "feature-x", "initiator": "claude" }),
    );
    let second_id = second["session_id"].as_str().unwrap();
    assert!(
        app.active_collab_session_snapshot().is_none(),
        "status must not report one active session while multiple scopes are bound"
    );
    assert_eq!(
        app.active_collab_session_snapshot_for_scope("/repo", "main")
            .as_deref(),
        Some(first_id.as_str())
    );
    assert_eq!(
        app.active_collab_session_snapshot_for_scope("/repo", "feature-x")
            .as_deref(),
        Some(second_id)
    );
}

#[test]
fn collab_start_allows_restart_after_end() {
    let app = App::open_for_test().unwrap();
    // `drive_to_plan_locked` starts on /repo + main and reaches PlanLocked,
    // the phase where `collab_end` is permitted.
    let first_id = drive_to_plan_locked(&app, "fp");
    let ended = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &first_id, "agent": "claude" }),
    );
    assert_eq!(ended["ok"], true);

    // With the prior session explicitly ended, a fresh session on the same
    // repo+branch is allowed again.
    let second = call_tool(
        &app,
        "collab_start",
        json!({ "repo_path": "/repo", "branch": "main", "initiator": "claude" }),
    );
    assert_ne!(
        second["session_id"].as_str().unwrap(),
        first_id,
        "a new session after collab_end must have a distinct id"
    );
}

#[test]
fn collab_end_rejects_ineligible_active_planning_calls() {
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

    // Advance to PlanCodexReviewPending → PlanClaudeFinalizePending. The
    // finalize owner has a narrow abort path for oversized plans, but the
    // counterpart must still be rejected.
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
        json!({ "session_id": &session_id, "agent": "codex" }),
    );
    assert!(
        blocked["error"]
            .as_str()
            .unwrap_or("")
            .contains("PlanClaudeFinalizePending requires current owner claude"),
        "expected non-owner PlanClaudeFinalizePending rejection, got: {blocked}"
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
/// global review (`CodeReviewFixGlobalPending`, Codex-owned) under the v3
/// reorder.
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
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["last_head_sha"], "batch_head");

    // Global review_fix (Codex) → local audit (Claude) → final_review
    // (v3 reorder linear, terminal in 3 turns).
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
    assert_eq!(status["phase"], "CodeReviewLocalPending");

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
fn collab_v3_implementation_done_jumps_to_global_review() {
    // v3 batch mode (reorder): a single `implementation_done` send transitions
    // `CodeImplementPending` → `CodeReviewFixGlobalPending` with Codex as owner.
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
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
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

    // Codex fires it and the phase advances to global review (Codex-owned
    // under v3 reorder: Codex reads the raw post-implementation diff first).
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
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
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
fn collab_start_accepts_pilot_codex_and_defaults_implementer_to_pilot() {
    // `pilot=codex` with `implementer` omitted must round-trip through
    // `collab_status` as pilot=codex AND implementer=codex — implementer's
    // default follows the resolved pilot, not the historical hardcoded
    // `claude`.
    //
    // Also pins the Task 4 creation-seed fix: `current_owner` used to be
    // hardcoded to `Agent::Claude` at session creation regardless of
    // `pilot`. The pilot drafts first at `PlanParallelDrafts`, so a
    // `pilot=codex` session must be born owned by codex, not claude.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(started["pilot"], "codex");
    assert_eq!(started["implementer"], "codex");
    let session_id = started["session_id"].as_str().unwrap();

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["pilot"], "codex");
    assert_eq!(status["implementer"], "codex");
    assert_eq!(status["current_owner"], "codex");
}

#[test]
fn collab_pilot_and_implementer_remain_independent_in_the_reverse_mixed_case() {
    // The inverse of the historical `pilot=claude, implementer=codex` route:
    // pilot owns planning and the two post-implementation audit turns, while
    // the independently selected implementer owns only the implementation.
    //
    // Also pins the Task 4 creation-seed fix (phase-aware invariant):
    // `current_owner` is seeded to the resolved `pilot` at birth (the pilot
    // drafts first), but that must NOT make ownership sticky to the pilot
    // all the way through planning — a split-role session (`pilot=codex`,
    // `implementer=claude`) starts codex-owned but must land back on
    // `claude`, the independent implementer, once it reaches
    // `CodeImplementPending`. This pins the state machine's existing
    // `next.current_owner = session.implementer` transition
    // (`state_machine/mod.rs`, the `PlanLocked` -> `SubmitTaskList` arm)
    // against a future regression introduced by the creation-seed change.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "pilot-codex-implementer-claude",
            "initiator": "claude",
            "pilot": "codex",
            "implementer": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();
    assert_eq!(started["pilot"], "codex");
    assert_eq!(started["implementer"], "claude");

    // Creation-seed: current_owner starts at the pilot (codex drafts first),
    // before any drafting has happened.
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["current_owner"], "codex");

    for (sender, content) in [("claude", "cdraft"), ("codex", "xdraft")] {
        call_tool(
            &app,
            "collab_send",
            json!({
                "session_id": &session_id,
                "sender": sender,
                "topic": "draft",
                "content": content,
            }),
        );
    }
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "canonical",
            "content": "canonical plan",
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string(),
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "final",
            "content": json!({ "plan": "final plan" }).to_string(),
        }),
    );
    let plan_hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "task_list",
            "content": task_list_payload(&plan_hash, "base", "head", 1),
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_owner"], "claude");

    let wrong_implementer = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "implemented" }).to_string(),
        }),
    );
    assert!(
        wrong_implementer.to_lowercase().contains("not your turn")
            || wrong_implementer.contains("expects sender"),
        "pilot must not be able to substitute for the independent implementer: {wrong_implementer}"
    );

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "implemented" }).to_string(),
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "claude");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": "reviewed" }).to_string(),
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "codex");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "review_local",
            "content": json!({ "head_sha": "reviewed" }).to_string(),
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "codex");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "final_review",
            "content": json!({ "head_sha": "reviewed", "pr_url": "https://example.test/pr/1" }).to_string(),
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodingComplete");
}

#[test]
fn collab_start_rejects_invalid_pilot_and_creates_no_session_row() {
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": "gemini"
        }),
    );
    assert!(
        err.contains("pilot") && err.contains("gemini"),
        "expected an error naming both the field and the bad value, got: {err}"
    );

    // Prove the rejection happened before any write, not merely that the
    // call returned an error: query `collab_sessions` directly.
    let count: i64 = app
        .db
        .with_transaction(|tx| {
            Ok(tx.query_row("SELECT COUNT(*) FROM collab_sessions", [], |row| row.get(0))?)
        })
        .unwrap();
    assert_eq!(
        count, 0,
        "an invalid pilot must not create a collab_sessions row"
    );
}

#[test]
fn collab_start_pilot_absent_defaults_to_claude() {
    // Absent `pilot` key should default to `Agent::Claude`.
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
    assert_eq!(started["pilot"], "claude");
}

#[test]
fn collab_start_pilot_valid_string_is_accepted() {
    // Valid string `pilot` value should be accepted and used.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(started["pilot"], "codex");
}

#[test]
fn collab_start_pilot_number_is_rejected() {
    // Non-string `pilot` value (number) should be rejected.
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": 123
        }),
    );
    assert!(
        err.to_lowercase().contains("pilot"),
        "expected error to name 'pilot', got: {err}"
    );
    assert!(
        err.to_lowercase().contains("string"),
        "expected error to mention 'string', got: {err}"
    );
}

#[test]
fn collab_start_pilot_boolean_is_rejected() {
    // Non-string `pilot` value (boolean) should be rejected.
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": true
        }),
    );
    assert!(
        err.to_lowercase().contains("pilot"),
        "expected error to name 'pilot', got: {err}"
    );
    assert!(
        err.to_lowercase().contains("string"),
        "expected error to mention 'string', got: {err}"
    );
}

#[test]
fn collab_start_pilot_explicit_null_is_rejected() {
    // Explicit `null` for `pilot` should be rejected (not treated as absent).
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": null
        }),
    );
    assert!(
        err.to_lowercase().contains("pilot"),
        "expected error to name 'pilot', got: {err}"
    );
    assert!(
        err.to_lowercase().contains("string"),
        "expected error to mention 'string', got: {err}"
    );
}

#[test]
fn collab_start_implementer_absent_defaults_to_pilot() {
    // Absent `implementer` key should default to the resolved `pilot`.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(started["pilot"], "codex");
    assert_eq!(started["implementer"], "codex");

    // Also test with absent pilot (so both default).
    let started2 = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo2",
            "branch": "main",
            "initiator": "claude"
        }),
    );
    assert_eq!(started2["pilot"], "claude");
    assert_eq!(started2["implementer"], "claude");
}

#[test]
fn collab_start_implementer_valid_string_is_accepted() {
    // Valid string `implementer` value should be accepted and used.
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "implementer": "codex"
        }),
    );
    assert_eq!(started["implementer"], "codex");
}

#[test]
fn collab_start_implementer_number_is_rejected() {
    // Non-string `implementer` value (number) should be rejected.
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "implementer": 456
        }),
    );
    assert!(
        err.to_lowercase().contains("implementer"),
        "expected error to name 'implementer', got: {err}"
    );
    assert!(
        err.to_lowercase().contains("string"),
        "expected error to mention 'string', got: {err}"
    );
}

#[test]
fn collab_start_implementer_boolean_is_rejected() {
    // Non-string `implementer` value (boolean) should be rejected.
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "implementer": false
        }),
    );
    assert!(
        err.to_lowercase().contains("implementer"),
        "expected error to name 'implementer', got: {err}"
    );
    assert!(
        err.to_lowercase().contains("string"),
        "expected error to mention 'string', got: {err}"
    );
}

#[test]
fn collab_start_implementer_explicit_null_is_rejected() {
    // Explicit `null` for `implementer` should be rejected (not treated as absent).
    let app = App::open_for_test().unwrap();
    let err = call_tool_expect_error(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "implementer": null
        }),
    );
    assert!(
        err.to_lowercase().contains("implementer"),
        "expected error to name 'implementer', got: {err}"
    );
    assert!(
        err.to_lowercase().contains("string"),
        "expected error to mention 'string', got: {err}"
    );
}

#[test]
fn collab_set_implementer_before_task_list_routes_batch_owner() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");

    let updated = call_tool(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "implementer": "codex"
        }),
    );
    assert_eq!(updated["implementer"], "codex");
    assert_eq!(updated["phase"], "PlanLocked");

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
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["implementer"], "codex");
    assert_eq!(status["current_owner"], "codex");
}

#[test]
fn collab_set_implementer_during_batch_moves_current_owner() {
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
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["current_owner"], "claude");

    let updated = call_tool(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "implementer": "codex"
        }),
    );
    assert_eq!(updated["phase"], "CodeImplementPending");
    assert_eq!(updated["implementer"], "codex");
    assert_eq!(updated["current_owner"], "codex");

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
}

#[test]
fn collab_set_implementer_rejects_after_batch_implementation() {
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
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "batch_head" }).to_string()
        }),
    );

    let err = call_tool_expect_error(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "implementer": "codex"
        }),
    );
    assert!(
        err.contains("before implementation is complete"),
        "expected post-implementation rejection, got: {err}"
    );
}

/// Number of `wal_log` rows for the given `operation` whose `params` blob
/// mentions this `session_id`. Rejection tests use this to prove a refused
/// call wrote nothing to the audit trail, not merely that it returned an
/// error.
fn wal_row_count(app: &App, session_id: &str, operation: &str) -> i64 {
    let pattern = format!("%\"session_id\":\"{session_id}\"%");
    app.db
        .with_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT COUNT(*) FROM wal_log WHERE operation = ?1 AND params LIKE ?2",
                rusqlite::params![operation, pattern],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}

/// The most recent `wal_log` row for `operation`, as `(params, result)` JSON
/// values. Companion to `wal_row_count`: that helper proves a row exists;
/// this one proves what it actually says. Mirrors `last_approve_wal`, the
/// same pattern `collab_session.rs`'s own unit tests use to assert
/// `collab_approve`'s WAL payload — reused here (rather than reopening a
/// fresh connection to `app.config.db_path`, as `last_approve_wal` does) so
/// it also works against `App::open_for_test`'s in-memory database, which a
/// second connection to the same path would not see.
fn last_wal_row(app: &App, operation: &str) -> (serde_json::Value, serde_json::Value) {
    app.db
        .with_transaction(|tx| {
            let (params, result): (String, String) = tx.query_row(
                "SELECT params, result FROM wal_log WHERE operation = ?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params![operation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok((
                serde_json::from_str(&params).unwrap(),
                serde_json::from_str(&result).unwrap(),
            ))
        })
        .unwrap()
}

#[test]
fn collab_set_implementer_rejects_non_pilot_caller() {
    // The pilot defaults to `claude` (see `handle_collab_start`); `codex` is
    // therefore not the pilot here and must be refused, mirroring
    // `collab_set_pilot`'s permitted-caller rule.
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");

    let before = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    let wal_before = wal_row_count(&app, &session_id, "collab_set_implementer");

    let err = call_tool_expect_error(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "implementer": "codex"
        }),
    );
    assert!(
        err.contains("codex") && err.contains("pilot 'claude'"),
        "expected a rejection naming the caller and the current pilot, got: {err}"
    );

    let after = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(
        after["implementer"], before["implementer"],
        "a refused collab_set_implementer must leave implementer unchanged"
    );
    assert_eq!(
        after["current_owner"], before["current_owner"],
        "a refused collab_set_implementer must leave current_owner unchanged"
    );
    assert_eq!(
        after["updated_at"], before["updated_at"],
        "a refused collab_set_implementer must not touch updated_at"
    );
    assert_eq!(
        wal_row_count(&app, &session_id, "collab_set_implementer"),
        wal_before,
        "a refused collab_set_implementer must write no WAL row"
    );
}

#[test]
fn collab_set_implementer_allows_current_pilot_caller_pre_lock() {
    // The permitted-caller rule is role-generic, not Claude-flavoured: under
    // `pilot=codex` it is Codex — and only Codex — who may reassign the
    // implementer, here in the earliest phase the gate allows
    // (`PlanParallelDrafts`, before any task list exists).
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "feat/set-implementer-pilot-caller",
            "initiator": "claude",
            "pilot": "codex"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanParallelDrafts");

    let updated = call_tool(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "implementer": "claude"
        }),
    );
    assert_eq!(updated["implementer"], "claude");
    assert_eq!(updated["phase"], "PlanParallelDrafts");
}

/// Start a fresh planning session with an explicit `pilot` and return its id.
/// The session lands in `PlanParallelDrafts` with neither draft submitted —
/// the only state in which `collab_set_pilot` is ever allowed.
fn start_session_with_pilot(app: &App, branch: &str, pilot: &str) -> String {
    let started = call_tool(
        app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": branch,
            "initiator": "claude",
            "pilot": pilot
        }),
    );
    assert_eq!(started["pilot"], pilot);
    started["session_id"].as_str().unwrap().to_string()
}

/// Re-read the persisted session row and return `(pilot, current_owner)`.
/// Rejection tests use this to prove a refused `collab_set_pilot` left zero
/// mutation behind rather than merely returning an error.
fn pilot_and_owner(app: &App, session_id: &str) -> (String, String) {
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    (
        status["pilot"].as_str().unwrap().to_string(),
        status["current_owner"].as_str().unwrap().to_string(),
    )
}

#[test]
fn collab_set_pilot_reassigns_pilot_and_owner_before_any_draft() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/set-pilot-ok", "claude");

    let updated = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(updated["phase"], "PlanParallelDrafts");
    assert_eq!(updated["pilot"], "codex");
    // `current_owner` moves with the pilot in the same UPDATE.
    assert_eq!(updated["current_owner"], "codex");

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        ("codex".to_string(), "codex".to_string()),
        "the reassignment must be persisted, not merely reported"
    );
}

#[test]
fn collab_set_pilot_reassigns_from_codex_back_to_claude() {
    // The permitted-caller rule is role-generic, not Claude-flavoured: under
    // `pilot=codex` it is Codex — and only Codex — who may hand the role over.
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/set-pilot-ok-codex", "codex");

    let updated = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "pilot": "claude"
        }),
    );
    assert_eq!(updated["pilot"], "claude");
    assert_eq!(updated["current_owner"], "claude");

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        ("claude".to_string(), "claude".to_string())
    );
}

#[test]
fn collab_set_pilot_rejects_after_a_draft_has_landed() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/set-pilot-drafted", "claude");
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "cdraft"
        }),
    );
    let before = pilot_and_owner(&app, &session_id);

    let err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert!(
        err.contains("PlanParallelDrafts") && err.contains("draft"),
        "expected a rejection naming the phase and the landed draft, got: {err}"
    );

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        before,
        "a refused collab_set_pilot must leave pilot and current_owner unchanged"
    );
}

#[test]
fn collab_set_pilot_rejects_after_batch_implementation() {
    let app = App::open_for_test().unwrap();
    let session_id = drive_to_plan_locked(&app, "fp");
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", "h0", 1)
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": "batch_head" }).to_string()
        }),
    );
    let before = pilot_and_owner(&app, &session_id);

    // The caller IS the current pilot here, so this exercises the phase gate
    // on its own rather than tripping the permitted-caller rule first.
    let err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert!(
        err.contains("CodeReviewFixGlobalPending") && err.contains("PlanParallelDrafts"),
        "expected a rejection naming the current phase and the only allowed one, got: {err}"
    );

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        before,
        "a refused collab_set_pilot must leave pilot and current_owner unchanged"
    );
}

#[test]
fn collab_set_pilot_rejects_on_a_code_review_session() {
    // A `collab_start_code_review` session begins at
    // `CodeReviewFixGlobalPending` and so never passes through the one phase
    // where the pilot is reassignable — its pilot is fixed at creation.
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, _descendant_sha, _drift_sha) = git_repo_fixture();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/set-pilot-review",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();
    let before = pilot_and_owner(&app, &session_id);

    let err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert!(
        err.contains("CodeReviewFixGlobalPending") && err.contains("PlanParallelDrafts"),
        "expected a rejection naming the current phase and the only allowed one, got: {err}"
    );

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        before,
        "a refused collab_set_pilot must leave pilot and current_owner unchanged"
    );
}

#[test]
fn collab_set_pilot_refuses_copilot_self_promotion_under_pilot_claude() {
    // Turn-seizure attempt: the copilot (codex here) naming itself pilot in
    // the earliest, most permissive state. Refused on the caller rule.
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/seize-under-claude", "claude");
    let before = pilot_and_owner(&app, &session_id);

    let err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "pilot": "codex"
        }),
    );
    assert!(
        err.contains("copilot") && err.contains("pilot 'claude'"),
        "expected a rejection naming the caller's role and the current pilot, got: {err}"
    );

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        before,
        "a refused collab_set_pilot must leave pilot and current_owner unchanged"
    );
}

#[test]
fn collab_set_pilot_refuses_copilot_self_promotion_under_pilot_codex() {
    // Mirror of the above with the roles swapped: under `pilot=codex` the
    // copilot is Claude, and Claude promoting itself is refused identically.
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/seize-under-codex", "codex");
    let before = pilot_and_owner(&app, &session_id);

    let err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "claude"
        }),
    );
    assert!(
        err.contains("copilot") && err.contains("pilot 'codex'"),
        "expected a rejection naming the caller's role and the current pilot, got: {err}"
    );

    assert_eq!(
        pilot_and_owner(&app, &session_id),
        before,
        "a refused collab_set_pilot must leave pilot and current_owner unchanged"
    );
}

/// Re-read the persisted session row and return `(pilot, implementer,
/// current_owner)`. Used by the Task 9 handoff-staleness tests below to prove
/// a refused role mutation left all three role fields untouched, not merely
/// the two `pilot_and_owner` checks — `collab_set_implementer` is one of the
/// tools under test below, so its own target field must be pinned too.
fn full_roles(app: &App, session_id: &str) -> (String, String, String) {
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    (
        status["pilot"].as_str().unwrap().to_string(),
        status["implementer"].as_str().unwrap().to_string(),
        status["current_owner"].as_str().unwrap().to_string(),
    )
}

// ── Task 9 audit finding ────────────────────────────────────────────────────
//
// AUDIT FINDING (Task 9: "audit reassignment against handoff staleness"):
// pilot/implementer reassignment (`collab_set_pilot`, `collab_set_implementer`)
// does NOT advance any actor's generation and does NOT invalidate any
// outstanding `session_handoff` token. `collab_actor_generations` (the
// generation/token lease table) is keyed by `(session_id, agent)` and models
// process succession for a given agent identity — "has a fresh process
// claimed the right to act as this agent" — which is an orthogonal concern
// to `pilot`/`implementer`/`current_owner`, which model *role* state. Both
// `handle_collab_set_pilot` and `handle_collab_set_implementer` route through
// the shared `ensure_caller_is_current_pilot` preamble
// (`crates/ironmem/src/mcp/tools/collab_session.rs`), which does touch
// `collab_actor_generations` on every call via `ensure_actor_generation_current`
// (`crates/ironmem/src/mcp/tools/handoff.rs`) — it always reads the caller's
// row, and writes to it when the request carries a `handoff_token`. What it
// does NOT do is couple that read/write to the role-reassignment logic itself:
// whether `pilot`/`implementer`/`current_owner` gets changed by the call has
// no bearing on whether a generation advances, and vice versa — the two are
// driven by orthogonal inputs (the `handoff_token` field vs. the `pilot`/
// `implementer` field). So a token minted for an agent before a reassignment
// remains just as claimable after one. There is no such thing, in the current
// implementation, as "a handoff token invalidated specifically by a role
// reassignment."
//
// The task's acceptance criteria ("attempt a role mutation using the
// pre-reassignment handoff token — refused, zero mutation") is nonetheless
// satisfied *in practice* — but via the pre-existing "caller must equal the
// current pilot" identity check in `ensure_caller_is_current_pilot`
// (`crates/ironmem/src/mcp/tools/collab_session.rs`), not via any
// staleness/generation mechanism coupled to role state. The two tests below
// pin both halves of this finding: (1) the token guard genuinely does reject
// reuse of an already-*spent* token, on its own, independent of role state;
// and (2) the literal "present a still-valid, never-spent, pre-reassignment
// token after the pilot has moved on" scenario is refused by the *caller
// identity* check, not by any token-staleness check — because no such check
// exists.
//
// Not in scope here (tracked separately, per the plan's scope exclusions):
// `ensure_actor_generation_current` calls `app.set_cached_generation` right
// after `claim_handoff_token` succeeds but before the enclosing transaction
// commits (see `crates/ironmem/src/mcp/tools/handoff.rs`); if that
// transaction later rolls back (e.g. because the caller-identity check below
// it then fails, as happens in the second test here), the in-process cache
// can end up one generation ahead of the database. That is a
// transaction/retry-behavior class issue, not a reassignment/staleness one,
// and is intentionally left unfixed by this task.

/// Task 9, scenario 1 (genuine spent-token reuse): a `session_handoff` token
/// is a one-time credential. Mint T1 for claude, spend it on a *permitted*
/// same-agent no-op `collab_set_pilot` call (claude -> claude — still legal
/// in `PlanParallelDrafts` with no draft landed), then reuse the same T1 for
/// a second call **by the same agent** (claude). Reusing it as the same
/// caller isolates the token guard: the pilot-identity check would pass
/// (claude is still pilot), so only `claim_handoff_token` finding the token
/// already spent can be the source of the refusal.
#[test]
fn collab_set_pilot_spent_handoff_token_reuse_by_same_agent_is_refused_with_no_mutation() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/spent-handoff-reuse", "claude");

    // Mint a handoff token for claude — this is T1.
    let issued = call_tool(
        &app,
        "session_handoff",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    let token = issued["handoff_token"].as_str().unwrap().to_string();

    // Spend T1 on a permitted same-agent no-op reassignment (claude -> claude).
    // `handle_collab_set_pilot`'s Rule 3 UPDATE is unconditional even when
    // `previous == pilot`, so this is a legal, real claim of T1.
    let spent = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "claude",
            "handoff_token": &token
        }),
    );
    assert_eq!(spent["pilot"], "claude");
    assert_eq!(spent["current_owner"], "claude");

    let before = full_roles(&app, &session_id);
    assert_eq!(
        before,
        (
            "claude".to_string(),
            "claude".to_string(),
            "claude".to_string()
        ),
        "sanity: claude remains pilot/implementer/owner after the no-op spend"
    );

    // Reuse the SAME (now-spent) token T1 for a second call, still as
    // claude — the current pilot. If the token guard were missing, this call
    // would otherwise be fully authorized (claude == current pilot) and
    // would succeed; only the spent-token guard can refuse it.
    let err = call_tool_expect_error(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "implementer": "codex",
            "handoff_token": &token
        }),
    );
    assert_eq!(
        err, "handoff_token already claimed",
        "expected the exact spent-token refusal from claim_handoff_token, got: {err}"
    );

    assert_eq!(
        full_roles(&app, &session_id),
        before,
        "a refused spent-token role mutation must leave pilot, implementer, \
         and current_owner unchanged"
    );
}

/// Task 9, scenario 2 (the literal acceptance-criteria scenario): mint T1 for
/// claude and leave it **unspent**. Reassign the pilot away from claude
/// (claude -> codex) through a *separate, tokenless* `collab_set_pilot` call
/// — legal because claude's actual generation is still 0 (issuing a token
/// never advances it; only claiming one does). Then present the
/// still-valid, never-claimed T1 as claude in a subsequent role-mutation
/// call. `claim_handoff_token` actually *succeeds* here (T1 is genuinely
/// still pending and unclaimed for claude) — the refusal comes entirely from
/// the downstream "caller is not the current pilot" check, since claude is
/// no longer pilot. This is the mechanism the audit finding above documents:
/// no staleness/generation check fires here, only the pre-existing
/// caller-identity check. (The successful claim inside this failed call's
/// transaction rolls back along with everything else, per
/// `Database::with_transaction`'s "no commit on Err" behavior — so this
/// assertion of zero role mutation holds regardless.)
#[test]
fn collab_set_pilot_unspent_pre_reassignment_token_is_refused_by_caller_identity_not_token_guard() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/pre-reassignment-token", "claude");

    // Mint a handoff token for claude — this is T1 — and leave it unspent.
    let issued = call_tool(
        &app,
        "session_handoff",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    let token = issued["handoff_token"].as_str().unwrap().to_string();

    // Reassign the pilot away from claude via a SEPARATE, tokenless call.
    // T1 is untouched by this: the call's shared preamble still reads claude's
    // row in `collab_actor_generations` (via `ensure_actor_generation_current`,
    // since no `handoff_token` arg means it takes the read-only validation
    // path), but the *reassignment logic* itself never advances a generation
    // or invalidates a token as a side effect of changing `pilot` — that only
    // happens when a request separately supplies a `handoff_token` to claim.
    let reassigned = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(reassigned["pilot"], "codex");
    assert_eq!(reassigned["current_owner"], "codex");

    let before = full_roles(&app, &session_id);
    assert_eq!(
        before,
        (
            "codex".to_string(),
            "claude".to_string(),
            "codex".to_string()
        ),
        "sanity: the tokenless reassignment above must actually have landed"
    );

    // Present the still-unspent, pre-reassignment token T1 as claude. Refused
    // — but by the caller-identity check, not a token error.
    let err = call_tool_expect_error(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "implementer": "codex",
            "handoff_token": &token
        }),
    );
    assert_eq!(
        err,
        "collab_set_implementer refused: caller 'claude' is not the pilot of this \
         session; only the current pilot 'codex' may reassign the implementer",
        "expected the caller-identity refusal (T1 is still valid and claimable, so no \
         token error can fire here), got: {err}"
    );
    assert!(
        !err.contains("already claimed") && !err.contains("invalid handoff_token"),
        "this refusal must come from the caller-identity check, not a token error: {err}"
    );

    assert_eq!(
        full_roles(&app, &session_id),
        before,
        "a refused role mutation must leave pilot, implementer, and current_owner \
         unchanged, even though claiming T1 briefly succeeded inside the (rolled-back) \
         transaction"
    );
}

/// Task 9: after a permitted `collab_set_pilot` reassignment, turn
/// acquisition must resolve to exactly one claimable owner — never both
/// agents believing it is their turn, and never neither. `current_owner` is a
/// single-valued field and `wait_turn_snapshot`'s `is_my_turn` is a plain
/// equality against it, so this is a by-construction invariant; this test
/// pins it through the actual `collab_wait_my_turn` tool for both agents
/// rather than asserting on the internal snapshot type directly.
#[test]
fn collab_set_pilot_reassignment_leaves_exactly_one_claimable_owner() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/single-claimable-owner", "codex");

    // codex is pilot and current_owner initially; hand the pilot (and, in the
    // same UPDATE, current_owner) to claude.
    let reassigned = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "pilot": "claude"
        }),
    );
    assert_eq!(reassigned["pilot"], "claude");
    assert_eq!(reassigned["current_owner"], "claude");

    // The new owner (claude) must resolve its turn immediately.
    let claude_wait = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": &session_id, "agent": "claude", "timeout_secs": 1 }),
    );
    assert_eq!(claude_wait["is_my_turn"], true);
    assert_eq!(claude_wait["current_owner"], "claude");

    // The demoted agent (codex) must NOT resolve as its turn — it times out
    // unsettled rather than ever observing `is_my_turn: true`.
    let codex_wait = call_tool(
        &app,
        "collab_wait_my_turn",
        json!({ "session_id": &session_id, "agent": "codex", "timeout_secs": 1 }),
    );
    assert_eq!(
        codex_wait,
        json!({ "unchanged": true }),
        "the demoted agent must never observe is_my_turn: true after reassignment"
    );

    // Cross-check against collab_status: exactly one of the two agents'
    // identity matches current_owner.
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    let owner = status["current_owner"].as_str().unwrap();
    let claimable_count = ["claude", "codex"]
        .iter()
        .filter(|agent| **agent == owner)
        .count();
    assert_eq!(
        claimable_count, 1,
        "exactly one agent identity must match current_owner, got owner={owner}"
    );
}

#[test]
fn collab_set_pilot_writes_wal_row_with_operation_session_and_actor() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/set-pilot-wal-row", "claude");

    let updated = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(updated["pilot"], "codex");

    // `last_wal_row` already filters by operation name via its SQL WHERE
    // clause; the exact-equality assertion below additionally pins the
    // session id and the acting agent (`agent`, i.e. the actor) inside the
    // logged payload, plus every other field `handle_collab_set_pilot`
    // writes.
    let (params, _result) = last_wal_row(&app, "collab_set_pilot");
    assert_eq!(
        params,
        json!({
            "session_id": session_id,
            "agent": "claude",
            "previous_pilot": "claude",
            "pilot": "codex",
            "phase": "PlanParallelDrafts",
            "previous_owner": "claude",
            "current_owner": "codex",
            "changed": true,
        }),
        "the collab_set_pilot WAL row must record the operation's session id \
         and acting agent, matching the payload the handler actually writes"
    );
}

#[test]
fn collab_set_pilot_same_pilot_call_repairs_drifted_current_owner() {
    // Rule 3 in `handle_collab_set_pilot` moves `current_owner` to the named
    // `pilot` unconditionally, in the very same UPDATE as the pilot
    // assignment itself — even when `pilot` names the agent who is already
    // pilot. This proves the "unconditional" half specifically: manufacture
    // a session where `current_owner` has drifted away from `pilot` (the
    // public MCP surface cannot produce this starting from a fresh session;
    // it is constructed here directly against the DB, the same way the
    // `set_pilot` test helper in `collab_session.rs`'s own unit tests
    // rebinds session state for setup), then show a same-pilot call — a
    // request that does not change `pilot` at all — still repairs it.
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/set-pilot-repair-drift", "claude");

    let mut session = app.db.collab_load_session(&session_id).unwrap();
    assert_eq!(session.pilot, Agent::Claude);
    session.current_owner = Agent::Codex;
    app.db.collab_save_session(&session).unwrap();
    assert_eq!(
        pilot_and_owner(&app, &session_id),
        ("claude".to_string(), "codex".to_string()),
        "setup must produce a genuine drift: pilot and current_owner disagree"
    );

    let updated = call_tool(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "claude"
        }),
    );
    assert_eq!(
        updated["pilot"], "claude",
        "pilot did not change — this is the no-op reassignment case"
    );
    assert_eq!(
        updated["current_owner"], "claude",
        "a same-pilot call must still repair a drifted current_owner"
    );
    assert_eq!(
        pilot_and_owner(&app, &session_id),
        ("claude".to_string(), "claude".to_string()),
        "the repair must be persisted, not merely reported"
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
    assert_eq!(ok["phase"], "CodeReviewFixGlobalPending");
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
    let row = app
        .db
        .get_task_outcome(session_id)
        .unwrap()
        .expect("shortcut review must create task_outcomes row");
    assert_eq!(row.collab_session_id.as_deref(), Some(session_id));
    assert!(row.started_at.is_some());
    assert_eq!(row.review_rounds, 0);
    assert_eq!(
        app.active_collab_session_snapshot().as_deref(),
        Some(session_id)
    );

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
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["last_head_sha"], descendant_sha);
    let row = app.db.get_task_outcome(session_id).unwrap().unwrap();
    assert_eq!(
        row.review_rounds, 1,
        "shortcut rework→review entry increments review_rounds"
    );

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    let row = app.db.get_task_outcome(session_id).unwrap().unwrap();
    assert_eq!(
        row.review_rounds, 1,
        "review→review must not increment review_rounds again"
    );

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
    let row = app.db.get_task_outcome(session_id).unwrap().unwrap();
    assert!(row.done_at.is_some(), "final_review sets done_at");
    assert_eq!(row.pr_url.as_deref(), Some("https://example/pr/42"));
    assert!(
        row.outcome.is_none(),
        "final_review must leave outcome in-flight until collab_end"
    );

    call_tool(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    let row = app.db.get_task_outcome(session_id).unwrap().unwrap();
    assert_eq!(row.outcome.as_deref(), Some("merged"));
}

#[test]
fn test_shortcut_review_flows_through_audit() {
    // Regression for the v3 reorder: the shortcut (collab_start_code_review)
    // seeds at CodeReviewFixGlobalPending / Codex, then `review_fix_global`
    // advances to CodeReviewLocalPending (Claude's audit turn) before
    // CodeReviewFinalPending.
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, descendant_sha, _drift_sha) = git_repo_fixture();

    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut-audit",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "task": "shortcut audit"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    // Shortcut seeds at CodeReviewFixGlobalPending / Codex.
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");

    // Codex sends review_fix_global -> CodeReviewLocalPending / Claude
    // (the new audit gate before the PR turn).
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
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "claude");

    // Claude audits -> CodeReviewFinalPending / Claude.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "claude");

    // Claude PRs -> CodingComplete.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final_review",
            "content": json!({ "head_sha": descendant_sha, "pr_url": "https://example/pr" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodingComplete");
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
    // Under v3 reorder: review_fix_global advances to CodeReviewLocalPending
    // (Claude's audit turn) before reaching CodeReviewFinalPending.
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], descendant_sha);

    // Claude's review_local advances to CodeReviewFinalPending.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": descendant_sha }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "claude");

    let blocked = call_tool_expect_error(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert!(blocked.contains("active phase CodeReviewFinalPending"));
}

/// Under the v3 reorder, `/collab review` shortcut sessions advance
/// `CodeReviewFixGlobalPending → CodeReviewLocalPending → CodeReviewFinalPending`.
/// Claude's `review_local` at `CodeReviewLocalPending` produces a NEW head
/// (her audit commit on top of Codex's). The ancestry gate guards
/// `(CodeReviewFixGlobalPending, CodeReviewFixGlobal)` and also fires
/// for `(CodeReviewLocalPending, CodeReviewLocal)` so a non-descendant
/// `claude_head` is rejected with `branch_drift`.
#[test]
fn test_shortcut_review_local_ancestry_enforced() {
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, codex_head, claude_off_branch) = git_repo_fixture();

    // Build `claude_head` as a descendant of `codex_head` (Codex's fix commit),
    // simulating Claude's `review_local` audit commit on top of Codex's work.
    // `git_repo_fixture` leaves HEAD on the `drift` branch, so check out
    // `codex_head` first.
    git(&["checkout", &codex_head], &repo_path);
    let claude_head = commit_file(
        &repo_path,
        "branch.txt",
        "claude audit\n",
        "claude audit commit",
    );

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

    // Codex advances CodeReviewFixGlobalPending → CodeReviewLocalPending
    // by sending review_fix_global with a valid descendant head.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": codex_head }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], codex_head);

    // Claude attempts review_local with a head that is NOT a descendant of
    // codex_head. This must be rejected with `branch_drift`.
    let blocked = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": claude_off_branch }).to_string()
        }),
    );
    assert!(
        blocked.contains("branch_drift"),
        "expected branch_drift error from non-descendant review_local, got: {}",
        blocked
    );

    // Phase must NOT have advanced past CodeReviewLocalPending.
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], codex_head);

    // A valid descendant review_local then advances to CodeReviewFinalPending.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_local",
            "content": json!({ "head_sha": claude_head }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], claude_head);
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

/// End-to-end `pilot=codex` session driven through the real MCP tool-call
/// surface (`call_tool`), from `collab_start` all the way to `CodingComplete`,
/// asserting the owner at every single phase transition. This is the
/// integration-level counterpart to the unit-level `apply_event` pinning
/// suites in `collab/state_machine/tests.rs`: those prove the state-machine
/// logic is role-generic in isolation; this proves the assembled MCP layer
/// (start → send → status → handoff) carries a non-default pilot correctly
/// end to end, not just through a single hop.
///
/// The owner sequence below is read directly off the live `apply_event` arms
/// in `crates/ironmem/src/collab/state_machine/mod.rs`:
///   PlanParallelDrafts --draft(claude)--> owner=codex (counterpart(actor))
///     --draft(codex)--> PlanSynthesisPending, owner=codex (pilot)
///     --canonical(codex, pilot)--> PlanCopilotReviewPending, owner=claude (copilot)
///     --review(claude, copilot)--> PlanFinalizePending, owner=codex (pilot)
///     --final(codex, pilot)--> PlanLocked, owner=codex (unchanged by PublishFinal)
///     --task_list(codex, pilot)--> CodeImplementPending, owner=codex (implementer)
///     --implementation_done(codex, implementer)--> CodeReviewFixGlobalPending, owner=claude (copilot)
///     --review_fix_global(claude, copilot)--> CodeReviewLocalPending, owner=codex (pilot)
///     --review_local(codex, pilot)--> CodeReviewFinalPending, owner=codex (pilot, unchanged)
///     --final_review(codex, pilot)--> CodingComplete
#[test]
fn collab_pilot_codex_end_to_end_mcp_flow_reaches_coding_complete() {
    let app = App::open_for_test().unwrap();

    // `collab_start` with pilot=codex, implementer omitted: implementer must
    // default to the resolved pilot (codex), per Task 7.
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "main",
            "initiator": "claude",
            "pilot": "codex"
        }),
    );
    assert_eq!(started["pilot"], "codex");
    assert_eq!(started["implementer"], "codex");
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanParallelDrafts");
    assert_eq!(status["pilot"], "codex");

    // Blind drafts: claude submits first. Neither draft has landed yet, so
    // ownership just flips to the counterpart of whoever acted — a role-blind
    // identity split, independent of pilot.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "cdraft"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanParallelDrafts");
    assert_eq!(status["current_owner"], "codex");

    // Codex submits the second (and last) draft: both drafts are in, so
    // synthesis is the pilot's job — owner becomes codex (the pilot).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "xdraft"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanSynthesisPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["pilot"], "codex");

    // Codex (pilot) publishes the canonical plan -> owner flips to claude
    // (the copilot), who reviews it.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "canonical",
            "content": "canonical plan v1"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    // `Phase::PlanCopilotReviewPending`'s wire string is unchanged from the
    // historical hardcoded-Claude-pilot naming ("PlanCodexReviewPending" —
    // Task 3 renamed the variant, not the wire form, to avoid corrupting
    // stored sessions).
    assert_eq!(status["phase"], "PlanCodexReviewPending");
    assert_eq!(status["current_owner"], "claude");

    // Claude (copilot) submits its one-pass review -> owner flips back to
    // codex (the pilot), who finalizes.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    // Same wire-compatibility note as above: `PlanFinalizePending`'s wire
    // string stays "PlanClaudeFinalizePending".
    assert_eq!(status["phase"], "PlanClaudeFinalizePending");
    assert_eq!(status["current_owner"], "codex");

    // Codex (pilot) publishes the final execution-ready plan -> PlanLocked.
    // `PublishFinal` does not reassign `current_owner`, so it stays codex.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "final",
            "content": json!({ "plan": "final plan text" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanLocked");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["pilot"], "codex");

    let final_plan_hash = status["final_plan_hash"].as_str().unwrap().to_string();

    // Exposure check (mid-flow): a `session_handoff` call must render the
    // pilot in its handoff block, not just in `collab_status`.
    let handoff = call_tool(
        &app,
        "session_handoff",
        json!({ "session_id": &session_id, "agent": "codex" }),
    );
    let handoff_block = handoff["handoff_block"].as_str().unwrap();
    assert!(
        handoff_block.contains("pilot: codex"),
        "handoff block must carry a `pilot: codex` line, got:\n{handoff_block}"
    );

    // Codex (pilot) submits the task list -> CodeImplementPending, owner is
    // the session's implementer, which defaulted to codex.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "task_list",
            "content": task_list_payload(&final_plan_hash, "base0", "head0", 2)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["implementer"], "codex");

    // Codex (the implementer) reports the batch implementation done ->
    // CodeReviewFixGlobalPending, owner flips to claude (the copilot).
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
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "claude");

    // Claude (copilot) applies the global review fixes -> CodeReviewLocalPending,
    // owner flips to codex (the pilot).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": "h2" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewLocalPending");
    assert_eq!(status["current_owner"], "codex");

    // Codex (pilot) submits its local audit -> CodeReviewFinalPending; the
    // pilot hands to the pilot, so owner stays codex.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_local",
            "content": json!({ "head_sha": "h2" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["pilot"], "codex");

    // Codex (pilot) submits the final review with a PR URL -> CodingComplete.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "final_review",
            "content": json!({ "head_sha": "h2", "pr_url": "https://example/pr/1" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodingComplete");
    assert_eq!(status["pr_url"], "https://example/pr/1");
    assert_eq!(status["pilot"], "codex");

    // CodingComplete is terminal — collab_end must be accepted.
    let ended = call_tool(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert_eq!(ended["ok"], true);
}
