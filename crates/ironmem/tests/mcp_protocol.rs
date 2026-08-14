//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests call `dispatch` directly with an in-memory App (noop embedder,
//! no ONNX model required) and assert on the JSON-RPC response shape.

use ironmem::collab::Agent;
use ironmem::config::{Config, EmbedMode, McpAccessMode};
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

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

/// Run `git`, first scrubbing every inherited `GIT_*` environment variable —
/// an inherited `GIT_DIR`/`GIT_WORK_TREE` would otherwise make `git` operate
/// on (or report shas from) a different repo than the fixture at `cwd`,
/// silently. Same idiom as `review_diff.rs`'s `scrub_git_environment`.
fn git(args: &[&str], cwd: &Path) -> String {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    let output = command
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
    // Pinned off, not inherited: a developer machine with a working signing
    // key or a personal hooksPath configured globally masks what is fragile
    // on a CI runner with neither.
    git(&["config", "commit.gpgsign", "false"], &repo_path);
    git(&["config", "core.hooksPath", "/dev/null"], &repo_path);

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
    drive_to_plan_locked_full(app, final_plan, implementer, "/repo")
}

/// Same as `drive_to_plan_locked`, but seeded at a real `repo_path` instead
/// of the historical `"/repo"` placeholder. Every test that drives a session
/// past `CodeImplementPending` needs one of these now (issue #273 Task 8):
/// `implementation_done`/`review_fix_global`/`review_local`/`final_review`
/// are all git-ancestry-checked against `repo_path`, and a placeholder path
/// that resolves to nothing makes that check an operational failure rather
/// than the real refusal (or real success) the test means to exercise.
fn drive_to_plan_locked_in_repo(app: &App, final_plan: &str, repo_path: &Path) -> String {
    drive_to_plan_locked_full(app, final_plan, None, &repo_path.to_string_lossy())
}

fn drive_to_plan_locked_full(
    app: &App,
    final_plan: &str,
    implementer: Option<&str>,
    repo_path: &str,
) -> String {
    let mut start_args = json!({
        "repo_path": repo_path,
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

/// A checkpoint payload that satisfies every condition of the
/// `implementation_done` gate for a batch of `tasks` tasks at `head`.
fn batch_complete_checkpoint(
    session_id: &str,
    agent: &str,
    head: &str,
    tasks: u64,
) -> serde_json::Value {
    let completed = (1..=tasks)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    json!({
        "session_id": session_id,
        "agent": agent,
        "status": "batch_complete",
        "head_sha": head,
        "completed_task_ids": completed,
        "gates_result": "passed",
        "gates_sha": head,
        "gates_commands": "cargo test --workspace",
    })
}

/// File the checkpoint `implementation_done` now demands as proof (issue #273
/// Task 7), covering every task in the session's accepted task list.
///
/// The task count is read back from `collab_status` — the same single source
/// of truth the gate derives it from — so a test that changes its task list
/// cannot silently file a checkpoint that under-covers and then be refused for
/// a reason it was not written to exercise.
fn checkpoint_batch_complete(app: &App, session_id: &str, agent: &str, head: &str) {
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    let tasks = status["tasks_count"]
        .as_u64()
        .expect("a session at CodeImplementPending has a readable tasks_count");
    call_tool(
        app,
        "collab_checkpoint",
        batch_complete_checkpoint(session_id, agent, head, tasks),
    );
}

/// Send `implementation_done` from Claude, advancing the batch phase to
/// global review (`CodeReviewFixGlobalPending`, Codex-owned) under the v3
/// reorder. Files the proving checkpoint first, as a real implementer must.
fn do_implementation_done(app: &App, session_id: &str, head: &str) {
    checkpoint_batch_complete(app, session_id, "claude", head);
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
    // Issue #273 Task 8: every head reported past `CodeImplementPending` is
    // now git-ancestry-checked against the session's real repo, so this
    // needs a real chain of commits behind `head0` → `batch_head` → `h2`
    // instead of unrelated placeholder strings.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(3);
    let (head0, batch_head, h2) = (&shas[0], &shas[1], &shas[2]);
    let session_id = drive_to_plan_locked_in_repo(&app, "final plan text", &repo_path);
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
            "content": task_list_payload(&hash, "base0", head0, 2)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["tasks_count"], 2);
    assert_eq!(status["base_sha"], "base0");

    // Single batch send replaces the per-task loop.
    do_implementation_done(&app, &session_id, batch_head);
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(&status["last_head_sha"], batch_head);

    // Global review_fix (Codex) → local audit (Claude) → final_review
    // (v3 reorder linear, terminal in 3 turns).
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": h2 }).to_string()
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
            "content": json!({ "head_sha": h2 }).to_string()
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
            "content": json!({ "head_sha": h2, "pr_url": "https://example/pr/1" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodingComplete");
    assert_eq!(status["pr_url"], "https://example/pr/1");
    assert_eq!(&status["last_head_sha"], h2);

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
    //
    // Real repo (issue #273 Task 8): `implementation_done`'s head is now
    // git-ancestry-checked against `last_head_sha`, so `head0`/`batch_head`
    // must be real, order-respecting commits.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(2);
    let (head0, batch_head) = (&shas[0], &shas[1]);
    let session_id = drive_to_plan_locked_in_repo(&app, "fp", &repo_path);
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", head0, 3)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_owner"], "claude");

    do_implementation_done(&app, &session_id, batch_head);
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(&status["last_head_sha"], batch_head);
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
    //
    // Real repo (issue #273 Task 8): `implementation_done`'s head is now
    // git-ancestry-checked against `last_head_sha`.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(2);
    let (head0, batch_head) = (&shas[0], &shas[1]);
    let session_id =
        drive_to_plan_locked_full(&app, "fp", Some("codex"), &repo_path.to_string_lossy());

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
            "content": task_list_payload(&hash, "b0", head0, 2)
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
            "content": json!({ "head_sha": batch_head }).to_string()
        }),
    );
    assert!(
        err.to_lowercase().contains("not your turn") || err.contains("expects sender"),
        "expected turn-ownership error, got: {err}"
    );

    // Codex fires it and the phase advances to global review (Codex-owned
    // under v3 reorder: Codex reads the raw post-implementation diff first).
    checkpoint_batch_complete(&app, &session_id, "codex", batch_head);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "implementation_done",
            "content": json!({ "head_sha": batch_head }).to_string()
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
    //
    // Real repo (issue #273 Task 8): every head reported past
    // `CodeImplementPending` is now git-ancestry-checked.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(3);
    let (head, implemented, reviewed) = (&shas[0], &shas[1], &shas[2]);
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": repo_path.to_string_lossy(),
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
            "content": task_list_payload(&plan_hash, "base", head, 1),
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
            "content": json!({ "head_sha": implemented }).to_string(),
        }),
    );
    assert!(
        wrong_implementer.to_lowercase().contains("not your turn")
            || wrong_implementer.contains("expects sender"),
        "pilot must not be able to substitute for the independent implementer: {wrong_implementer}"
    );

    checkpoint_batch_complete(&app, &session_id, "claude", implemented);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": implemented }).to_string(),
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
            "content": json!({ "head_sha": reviewed }).to_string(),
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
            "content": json!({ "head_sha": reviewed }).to_string(),
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
            "content": json!({ "head_sha": reviewed, "pr_url": "https://example.test/pr/1" }).to_string(),
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
    // Real repo (issue #273 Task 8): `implementation_done`'s head is now
    // git-ancestry-checked, and this test needs that send to succeed.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(2);
    let (head0, batch_head) = (&shas[0], &shas[1]);
    let session_id = drive_to_plan_locked_in_repo(&app, "fp", &repo_path);
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", head0, 1)
        }),
    );
    checkpoint_batch_complete(&app, &session_id, "claude", batch_head);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": batch_head }).to_string()
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
    // Real repo (issue #273 Task 8): `implementation_done`'s head is now
    // git-ancestry-checked, and this test needs that send to succeed.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(2);
    let (head0, batch_head) = (&shas[0], &shas[1]);
    let session_id = drive_to_plan_locked_in_repo(&app, "fp", &repo_path);
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", head0, 1)
        }),
    );
    checkpoint_batch_complete(&app, &session_id, "claude", batch_head);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": batch_head }).to_string()
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
// The generation guard's transaction/retry behavior is covered separately by
// `refused_token_role_mutation_does_not_poison_tokenless_generation_cache`:
// if a post-claim validation rolls back the DB lease, the next tokenless
// action is still answered by the authoritative DB state instead of a
// generation that never committed. The claim reaches the advisory cache only
// once its transaction commits (`GenerationClaim`), and if an entry ever does
// lead the DB the guard *drops* it rather than rebinding it to `db_active` —
// a distinction that matters only once the DB is past generation 0, which is
// pinned by the unit test
// `rolled_back_claim_does_not_admit_claimant_at_incumbent_generation`
// (`crates/ironmem/src/mcp/tools/handoff.rs`).

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

/// A token claim happens before the role-authorization check, so a refused
/// token-bearing role mutation must roll back both the role mutation and the
/// generation lease. The process-local generation cache must follow that
/// rollback: a subsequent tokenless action from the same App must still be
/// accepted at generation 0.
///
/// Scoped deliberately to generation 0, where "the process may act" is the
/// correct answer for *any* process. It reaches that answer because the
/// rolled-back claim never reaches the cache at all (`GenerationClaim` is
/// published only after the transaction commits), and — should an entry ever
/// lead the DB anyway — because the guard drops it rather than rebinding it to
/// the DB value. Neither distinction is visible from here;
/// `rolled_back_claim_does_not_admit_claimant_at_incumbent_generation` and
/// `claim_refused_after_write_never_mutates_generation_cache`
/// (`crates/ironmem/src/mcp/tools/handoff.rs`) exist to pin them.
#[test]
fn refused_token_role_mutation_does_not_poison_tokenless_generation_cache() {
    let app = App::open_for_test().unwrap();
    let session_id = start_session_with_pilot(&app, "feat/rollback-cache", "claude");

    // Mint a token for claude while claude is the pilot, then move the pilot
    // away in a separate tokenless call. The token remains valid and unspent.
    let issued = call_tool(
        &app,
        "session_handoff",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    let token = issued["handoff_token"]
        .as_str()
        .expect("session_handoff must return a token")
        .to_string();
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

    // The claim succeeds inside this transaction, but the caller is no
    // longer the pilot, so the role mutation is rejected and the transaction
    // rolls back.
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
    assert!(
        err.contains("caller 'claude' is not the pilot"),
        "expected the post-claim caller-identity rejection, got: {err}"
    );

    let lease = app
        .db
        .with_connection(|conn| {
            conn.query_row(
                "SELECT generation, pending_handoff_token, pending_handoff_generation \
                 FROM collab_actor_generations WHERE session_id = ?1 AND agent = 'claude'",
                rusqlite::params![&session_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .map_err(ironmem::error::MemoryError::from)
        })
        .unwrap();
    assert_eq!(
        lease.0, 0,
        "the rolled-back claim must not advance the lease"
    );
    assert_eq!(
        lease.1.as_deref(),
        Some(token.as_str()),
        "the rolled-back claim must leave the token pending"
    );
    assert_eq!(
        lease.2,
        Some(1),
        "the pending generation must remain claimable after rollback"
    );

    // Parallel drafts permit either agent. This is a valid tokenless action
    // from the same App whose cache was touched by the failed claim.
    let draft = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "claude rollback-cache draft"
        }),
    );
    assert!(draft["message_id"].is_string(), "draft response: {draft}");
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

    // `changed` is audit metadata with no programmatic consumer — its only
    // job is telling an auditor reading `wal_log` whether this call actually
    // mutated state. It is computed as `previous != pilot || previous_owner
    // != pilot`: the pilot-changed disjunct is already covered by
    // `collab_set_pilot_writes_wal_row_with_operation_session_and_actor`
    // above, but the owner-drift-repaired disjunct — a same-pilot call whose
    // `pilot` field never moves, yet a real mutation (repairing the drifted
    // `current_owner`) still happened — was untested. A future
    // "simplification" of that expression down to just `previous != pilot`
    // would silently start mislabeling this repair as a no-op in the audit
    // trail, and nothing would catch it. Assert the WAL row genuinely
    // reflects the drift-then-repair, not just that `changed` happens to be
    // true for an unrelated reason.
    let (params, _result) = last_wal_row(&app, "collab_set_pilot");
    assert_eq!(
        params["previous_owner"], "codex",
        "must record the drifted owner this call repaired"
    );
    assert_eq!(
        params["current_owner"], "claude",
        "must record the repaired owner"
    );
    assert_eq!(
        params["changed"], true,
        "a same-pilot call that repairs a drifted current_owner is a real \
         mutation and must be logged as changed: true, not a no-op"
    );
}

#[test]
fn collab_v2_end_rejected_in_coding_active_phase() {
    // Real repo (issue #273 Task 8): `implementation_done`'s head is now
    // git-ancestry-checked, and this test needs that send to succeed.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(2);
    let (head0, h1) = (&shas[0], &shas[1]);
    let session_id = drive_to_plan_locked_in_repo(&app, "fp", &repo_path);
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", head0, 1)
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
    checkpoint_batch_complete(&app, &session_id, "claude", h1);
    let ok = call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": h1 }).to_string()
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

/// The `final_review` transition (`CodeReviewFinalPending` → `CodingComplete`)
/// is ancestry-checked too (issue #273 Task 8) — before Task 8 it was the one
/// v3 batch-flow transition with no ancestry check at all, in either the
/// shortcut or the normal flow: the old `shortcut_ancestry` computation only
/// matched `(CodeReviewFixGlobalPending, CodeReviewFixGlobal)` and
/// `(CodeReviewLocalPending, ReviewLocal)`. `drift_sha` shares only
/// `base_sha` with `descendant_sha` (the review-local head), so it is not a
/// descendant — the cleanest non-descendant available from `git_repo_fixture`
/// without building a second branch by hand.
#[test]
fn test_shortcut_final_review_ancestry_enforced() {
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, descendant_sha, drift_sha) = git_repo_fixture();

    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut-final-ancestry",
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
    assert_eq!(status["last_head_sha"], descendant_sha);

    let blocked = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final_review",
            "content": json!({ "head_sha": drift_sha, "pr_url": "https://example/pr/1" })
                .to_string()
        }),
    );
    assert!(
        blocked.contains("branch_drift:"),
        "expected branch_drift for a non-descendant final_review head, got: {blocked}"
    );

    // Phase must NOT have advanced past CodeReviewFinalPending, and the
    // rejected head must not have been recorded.
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFinalPending");
    assert_eq!(status["current_owner"], "claude");
    assert_eq!(status["last_head_sha"], descendant_sha);
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

/// A well-formed sha that names no real commit at all — the purest form of
/// the #273 incident: an agent that never reached any commit, reporting one
/// anyway. `git merge-base --is-ancestor` exits 128 for this case (not 1,
/// which is reserved for "resolves, but isn't an ancestor"), so without its
/// own detection it falls into the generic operational-failure message and
/// reads as broken tooling — inviting a recoverable `failure_report` that
/// parks and hands off the turn — rather than naming the caller's own
/// fabricated report as the defect.
#[test]
fn collab_start_code_review_rejects_a_nonexistent_head_sha() {
    let app = App::open_for_test().unwrap();
    let (_temp, repo_path, base_sha, head_sha, _descendant_sha, _drift_sha) = git_repo_fixture();

    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": repo_path,
            "branch": "feat/review-shortcut-nonexistent-head",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "initiator": "claude",
            "task": "review completed branch"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    let fabricated = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let blocked = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": fabricated }).to_string()
        }),
    );
    assert!(
        blocked.contains("branch_drift:"),
        "a fabricated head_sha must still classify as branch_drift, got: {blocked}"
    );
    assert!(
        blocked.contains("does not name a commit that exists"),
        "expected the nonexistent-commit diagnostic, not the generic operational \
         failure, got: {blocked}"
    );
    assert!(
        !blocked.contains("git ancestry validation failed"),
        "a fabricated sha must not be misdiagnosed as an operational git failure, \
         got: {blocked}"
    );
    assert!(
        blocked.contains(fabricated),
        "the diagnostic should name the offending sha, got: {blocked}"
    );

    // Phase must NOT have advanced, and the session's real last_head_sha
    // (from collab_start_code_review) must be untouched by the refused send.
    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["last_head_sha"], head_sha);
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
    // Real repo (issue #273 Task 8): every head reported past
    // `CodeImplementPending` is now git-ancestry-checked.
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(3);
    let (head0, batch_head, h2) = (&shas[0], &shas[1], &shas[2]);

    // `collab_start` with pilot=codex, implementer omitted: implementer must
    // default to the resolved pilot (codex), per Task 7.
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": repo_path.to_string_lossy(),
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
            "content": task_list_payload(&final_plan_hash, "base0", head0, 2)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["implementer"], "codex");

    // Codex (the implementer) reports the batch implementation done ->
    // CodeReviewFixGlobalPending, owner flips to claude (the copilot).
    checkpoint_batch_complete(&app, &session_id, "codex", batch_head);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "implementation_done",
            "content": json!({ "head_sha": batch_head }).to_string()
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
            "content": json!({ "head_sha": h2 }).to_string()
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
            "content": json!({ "head_sha": h2 }).to_string()
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
            "content": json!({ "head_sha": h2, "pr_url": "https://example/pr/1" }).to_string()
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

// ── Task 15: full verification sweep and reversed-role scenario ────────────
//
// One end-to-end MCP-level run of the plan's reversed-role scenario
// (docs/iron/plans/2026-08-10-collab-role-safety.md, Task 15), exercising
// every role-safety change from Tasks 1-14 together in a single flow rather
// than in isolation, plus a *live* cross-check against the Task 12 dashboard
// (`/api/sessions`, served by the real `ironmem dashboard` binary against the
// same on-disk DB the App writes to) and the Task 13 `collab_wait_my_turn`
// baseline. The dashboard's `list_sessions`/`CollabSessionSummary` live in a
// `pub(crate)` module, so a genuine HTTP round trip against the real binary —
// not a direct function call — is the only way an integration test outside
// the crate can exercise it; `App::open_for_test`'s in-memory DB can't be
// reopened by a second connection, so this test uses an on-disk DB instead.

/// Spawn the real `ironmem dashboard` binary against `db_path` on an
/// ephemeral port and return once its bound address is known. A trimmed
/// duplicate of `dashboard_server.rs`'s `spawn_dashboard` helper — test
/// binaries in `tests/` do not share code with each other.
struct DashboardHandle {
    child: Child,
    addr: String,
}

impl Drop for DashboardHandle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_dashboard(db_path: &Path) -> DashboardHandle {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ironmem"))
        .arg("dashboard")
        .arg("--db")
        .arg(db_path)
        .arg("--port")
        .arg("0")
        .arg("--json")
        .arg("--exit-on-stdin-close")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dashboard binary for Task 15's live cross-check");

    let stdout = child.stdout.take().expect("dashboard stdout must be piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("read dashboard startup line");
    let meta: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("bad dashboard startup json ({e}): {line}"));
    let url = meta["url"]
        .as_str()
        .expect("startup json must carry url")
        .to_string();
    let addr = url
        .strip_prefix("http://")
        .expect("dashboard url must be http")
        .to_string();

    // Drain stderr in the background so the child never blocks on a full pipe.
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut l = String::new();
            while r.read_line(&mut l).unwrap_or(0) > 0 {
                eprint!("[dashboard] {l}");
                l.clear();
            }
        });
    }

    DashboardHandle { child, addr }
}

/// Issue one raw `GET` over TCP and parse the JSON response body.
fn http_get_json(addr: &str, path: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("connect {addr}: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    let mut bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => panic!("read dashboard response: {e}"),
        }
    }
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("dashboard response json ({e}): {body}"))
}

/// Fetch `/api/sessions` and return the `CollabSessionSummary` entry for
/// `session_id`.
fn dashboard_session_summary(addr: &str, session_id: &str) -> serde_json::Value {
    let sessions = http_get_json(addr, "/api/sessions");
    sessions
        .as_array()
        .expect("dashboard /api/sessions must return a JSON array")
        .iter()
        .find(|s| s["id"] == session_id)
        .unwrap_or_else(|| {
            panic!("session {session_id} missing from dashboard listing: {sessions}")
        })
        .clone()
}

/// Open an on-disk `App` (noop embedder, trusted MCP mode) so a second,
/// independent read-only connection — the real dashboard binary — can see
/// the same committed rows. `App::open_for_test`'s in-memory DB cannot be
/// shared this way.
fn open_disk_app_for_dashboard_sweep() -> (tempfile::TempDir, PathBuf, App) {
    let dir = tempfile::tempdir().expect("temp dir must be creatable");
    let db_path = dir.path().join("collab.sqlite3");
    let state_dir = dir.path().join("state");
    let config = Config {
        db_path: db_path.clone(),
        model_dir: PathBuf::from("/nonexistent"),
        model_dir_explicit: true,
        state_dir,
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    };
    let app = App::new(config).expect("disk-backed App must open for Task 15's full sweep");
    (dir, db_path, app)
}

/// Open a SECOND, independent `App` against the same on-disk DB — models a
/// second agent's own local `ironmem` MCP server process sharing one
/// repository's DB, the deployment topology `ensure_actor_generation_current`
/// (`crates/ironmem/src/mcp/tools/handoff.rs`) is designed around: in
/// production each agent runs its own MCP server process, so driving this
/// scenario across two `App` instances over one shared on-disk DB is
/// arguably MORE faithful to that topology than routing both agents through
/// a single `App`. The second App is used to model those independent MCP
/// server processes, not as a workaround for generation-cache rollback.
fn open_second_disk_app(db_path: &Path) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().expect("temp dir must be creatable");
    let state_dir = dir.path().join("state");
    let config = Config {
        db_path: db_path.to_path_buf(),
        model_dir: PathBuf::from("/nonexistent"),
        model_dir_explicit: true,
        state_dir,
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    };
    let app = App::new(config)
        .expect("second disk-backed App must open for the fresh-process workaround");
    (dir, app)
}

/// Step 7 cross-check, `collab_wait_my_turn` half: the current owner must
/// observe its own turn immediately; the other agent must never observe
/// `is_my_turn: true` and instead time out unsettled — mirroring
/// `collab_set_pilot_reassignment_leaves_exactly_one_claimable_owner`'s
/// pattern, reused here as a running cross-check against `collab_status`
/// rather than a one-off assertion. `owner_app`/`non_owner_app` are separate
/// parameters (rather than one shared `App`) so a checkpoint can cross-check
/// an agent whose in-process generation cache lives in a different `App`
/// instance than the owner's — see `open_second_disk_app`.
fn assert_wait_turn_matches_status(
    owner_app: &App,
    non_owner_app: &App,
    session_id: &str,
    owner: &str,
    non_owner: &str,
    checkpoint: &str,
) {
    let owner_wait = call_tool(
        owner_app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": owner, "timeout_secs": 1 }),
    );
    assert_eq!(
        owner_wait["is_my_turn"], true,
        "{checkpoint}: collab_wait_my_turn must report {owner}'s own turn"
    );
    assert_eq!(owner_wait["current_owner"], owner);

    let non_owner_wait = call_tool(
        non_owner_app,
        "collab_wait_my_turn",
        json!({ "session_id": session_id, "agent": non_owner, "timeout_secs": 1 }),
    );
    assert_eq!(
        non_owner_wait,
        json!({ "unchanged": true }),
        "{checkpoint}: collab_wait_my_turn must never report {non_owner}'s turn while {owner} owns it"
    );
}

/// Step 7 cross-check, dashboard half: the live `/api/sessions` listing must
/// report the same `pilot`/`implementer`/`current_owner`/`phase` as
/// `collab_status` at the same checkpoint.
fn assert_dashboard_matches_status(
    addr: &str,
    session_id: &str,
    status: &serde_json::Value,
    checkpoint: &str,
) {
    let summary = dashboard_session_summary(addr, session_id);
    assert_eq!(
        summary["pilot"], status["pilot"],
        "{checkpoint}: dashboard pilot must match collab_status"
    );
    assert_eq!(
        summary["implementer"], status["implementer"],
        "{checkpoint}: dashboard implementer must match collab_status"
    );
    assert_eq!(
        summary["current_owner"], status["current_owner"],
        "{checkpoint}: dashboard current_owner must match collab_status"
    );
    assert_eq!(
        summary["phase"], status["phase"],
        "{checkpoint}: dashboard phase must match collab_status"
    );
}

/// Task 15's full scenario, run in the exact 7-step order specified by the
/// plan. `pilot=codex, implementer=claude` at start, reassigned to
/// `pilot=claude` mid-flight (step 2) — the reversed-role case the plan
/// exists to pin. Step numbering in the comments below matches the plan's
/// literal step list verbatim.
///
/// Note on step 6: the plan's own scenario never reassigns `implementer`
/// away from `claude` (it is fixed at `collab_start` in step 1 and never
/// touched again), and step 2 ends with `pilot=claude` too — so by
/// `CodeImplementPending` this run has `pilot == implementer == claude`,
/// not a genuinely split pair. The assertion below is implemented exactly as
/// the plan specifies ("Assert `current_owner == claude`") rather than
/// silently strengthened into a split-role case; the split-role invariant
/// itself already has a dedicated regression guard from Task 4's tests
/// (`collab_start_accepts_pilot_codex_and_defaults_implementer_to_pilot` and
/// neighbors).
#[test]
fn collab_role_safety_full_verification_sweep_reversed_role_scenario() {
    let (_dir, db_path, app) = open_disk_app_for_dashboard_sweep();
    let dashboard = spawn_dashboard(&db_path);

    // ── Step 1 ──────────────────────────────────────────────────────────
    // collab_start --pilot=codex --implementer=claude. current_owner must
    // seed from the PILOT (Task 4's invariant), not the implementer.
    let started = call_tool(
        &app,
        "collab_start",
        json!({
            "repo_path": "/repo",
            "branch": "feat/role-safety-sweep",
            "initiator": "claude",
            "pilot": "codex",
            "implementer": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap().to_string();
    assert_eq!(started["pilot"], "codex");
    assert_eq!(started["implementer"], "claude");

    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["pilot"], "codex");
    assert_eq!(status["implementer"], "claude");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["phase"], "PlanParallelDrafts");

    // Step 7 checkpoint (post step 1).
    assert_wait_turn_matches_status(&app, &app, &session_id, "codex", "claude", "after step 1");
    assert_dashboard_matches_status(&dashboard.addr, &session_id, &status, "after step 1");

    // Mint a handoff token for codex BEFORE step 2's reassignment, and hold
    // it unspent through steps 2-3 for step 4's retry.
    let handoff = call_tool(
        &app,
        "session_handoff",
        json!({ "session_id": &session_id, "agent": "codex" }),
    );
    let pre_reassignment_token = handoff["handoff_token"]
        .as_str()
        .expect("session_handoff must return a token")
        .to_string();

    // ── Step 2 ──────────────────────────────────────────────────────────
    // As Codex (the CURRENT pilot), before any draft lands: reassign the
    // pilot to Claude. Must succeed; current_owner moves with it.
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

    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["pilot"], "claude");
    assert_eq!(status["implementer"], "claude");
    assert_eq!(status["current_owner"], "claude");

    // Step 7 checkpoint (post step 2).
    assert_wait_turn_matches_status(&app, &app, &session_id, "claude", "codex", "after step 2");
    assert_dashboard_matches_status(&dashboard.addr, &session_id, &status, "after step 2");

    // ── Step 3 ──────────────────────────────────────────────────────────
    // As Codex (now the FORMER pilot): both collab_set_pilot and
    // collab_set_implementer must be refused with the matrix's
    // authorization error, and pilot/implementer/current_owner/updated_at
    // must all be unchanged after BOTH attempts.
    let before = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));

    let set_pilot_auth_err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "pilot": "codex"
        }),
    );
    assert_eq!(
        set_pilot_auth_err,
        "collab_set_pilot refused: caller 'codex' is the copilot of this session; \
         only the current pilot 'claude' may reassign the pilot role"
    );

    let set_implementer_auth_err = call_tool_expect_error(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "implementer": "codex"
        }),
    );
    assert_eq!(
        set_implementer_auth_err,
        "collab_set_implementer refused: caller 'codex' is not the pilot of this session; \
         only the current pilot 'claude' may reassign the implementer"
    );

    let after_step3 = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(after_step3["pilot"], before["pilot"]);
    assert_eq!(after_step3["implementer"], before["implementer"]);
    assert_eq!(after_step3["current_owner"], before["current_owner"]);
    assert_eq!(
        after_step3["updated_at"], before["updated_at"],
        "step 3: two refused authorization attempts must not touch updated_at"
    );

    // ── Step 4 ──────────────────────────────────────────────────────────
    // Retry step 3's mutations using the token minted for codex BEFORE the
    // reassignment. Still refused — by the caller-identity check, not a
    // token-staleness mechanism (Task 9's finding: reassignment never
    // invalidates an outstanding token) — with zero mutation. Reusing the
    // SAME token for both retries is safe: `claim_handoff_token` succeeds
    // inside each call's own transaction, but the caller-identity check that
    // runs immediately after it fails, so the whole transaction (including
    // the token claim) rolls back every time and the token is never actually
    // spent.
    let retry_set_implementer_err = call_tool_expect_error(
        &app,
        "collab_set_implementer",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "implementer": "codex",
            "handoff_token": &pre_reassignment_token
        }),
    );
    assert_eq!(retry_set_implementer_err, set_implementer_auth_err);
    assert!(
        !retry_set_implementer_err.contains("already claimed")
            && !retry_set_implementer_err.contains("invalid handoff_token"),
        "step 4's refusal must come from the caller-identity check, not a token error: \
         {retry_set_implementer_err}"
    );

    let retry_set_pilot_err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "codex",
            "pilot": "codex",
            "handoff_token": &pre_reassignment_token
        }),
    );
    assert_eq!(retry_set_pilot_err, set_pilot_auth_err);
    assert!(
        !retry_set_pilot_err.contains("already claimed")
            && !retry_set_pilot_err.contains("invalid handoff_token"),
        "step 4's refusal must come from the caller-identity check, not a token error: \
         {retry_set_pilot_err}"
    );

    let after_step4 = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(after_step4["pilot"], before["pilot"]);
    assert_eq!(after_step4["implementer"], before["implementer"]);
    assert_eq!(after_step4["current_owner"], before["current_owner"]);
    assert_eq!(
        after_step4["updated_at"], before["updated_at"],
        "step 4: a refused token-bearing retry must not touch updated_at either"
    );

    // ── Step 5 ──────────────────────────────────────────────────────────
    // Land a draft, then attempt collab_set_pilot as Claude — the ACTUAL
    // current pilot post-step-2 — again. Refused on the PHASE gate this
    // time: a genuinely distinct error from step 3's authorization refusal.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "draft",
            "content": "claude blind draft"
        }),
    );

    let phase_gate_err = call_tool_expect_error(
        &app,
        "collab_set_pilot",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "pilot": "codex"
        }),
    );
    assert!(
        phase_gate_err.contains("PlanParallelDrafts") && phase_gate_err.contains("draft"),
        "expected the phase-gate rejection naming the phase and the landed draft, got: \
         {phase_gate_err}"
    );
    assert_ne!(
        phase_gate_err, set_pilot_auth_err,
        "step 5's phase-gate refusal must be textually distinct from step 3's authorization \
         refusal"
    );

    // ── Step 6 ──────────────────────────────────────────────────────────
    // Continue driving to CodeImplementPending. current_owner must equal the
    // IMPLEMENTER, not merely the (also-claude, post-reassignment) pilot —
    // see the file-level note above on why this scenario doesn't reach a
    // genuinely split pilot != implementer pair at this checkpoint.
    //
    // Codex's remaining actions in this scenario (submitting its own blind
    // draft, reviewing the canonical plan) route through a SECOND, independent
    // `App` rather than the original one — modeling codex's own local MCP
    // server process, per `open_second_disk_app`'s doc comment above.
    let (_dir2, app_codex) = open_second_disk_app(&db_path);

    call_tool(
        &app_codex,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "draft",
            "content": "codex blind draft"
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanSynthesisPending");
    assert_eq!(status["current_owner"], "claude");

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "canonical plan v1"
        }),
    );
    call_tool(
        &app_codex,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string()
        }),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": "final plan text" }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanLocked");
    let final_plan_hash = status["final_plan_hash"].as_str().unwrap().to_string();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&final_plan_hash, "base0", "head0", 1)
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    assert_eq!(
        status["current_owner"], "claude",
        "current_owner at CodeImplementPending must equal the implementer"
    );
    assert_eq!(status["pilot"], "claude");
    assert_eq!(status["implementer"], "claude");

    // Step 7 checkpoint (step 6's final state). Codex's half again goes
    // through `app_codex` — see the step 6 comment above.
    assert_wait_turn_matches_status(
        &app,
        &app_codex,
        &session_id,
        "claude",
        "codex",
        "at CodeImplementPending",
    );
    assert_dashboard_matches_status(
        &dashboard.addr,
        &session_id,
        &status,
        "at CodeImplementPending",
    );
}

// ── collab_checkpoint (issue #273 Task 5) ───────────────────────────────────

/// Start a session bound to `repo_path`/`branch` and return its id.
///
/// `collab_checkpoint` is phase-independent — it records progress, it does not
/// advance the state machine — so these tests deliberately skip the drive to
/// `CodeImplementPending` and check the checkpoint path on its own.
fn start_checkpoint_session(app: &App, repo_path: &str, branch: &str) -> String {
    let started = call_tool(
        app,
        "collab_start",
        json!({ "repo_path": repo_path, "branch": branch, "initiator": "claude" }),
    );
    started["session_id"]
        .as_str()
        .expect("collab_start returns a session_id")
        .to_string()
}

/// The stored checkpoint row, read back through the same loader the gate in
/// Tasks 7-10 will use — so these tests assert on what a later reader sees,
/// not merely on what the tool returned.
fn stored_checkpoint(app: &App, session_id: &str) -> Option<ironmem::collab::CollabCheckpoint> {
    app.db
        .with_connection(|conn| ironmem::collab::queue::load_current_checkpoint(conn, session_id))
        .expect("checkpoint load must not error")
}

/// A repo whose HEAD is a known SHA, plus that SHA.
fn checkpoint_repo() -> (tempfile::TempDir, String, String) {
    let temp = tempfile::tempdir().expect("temp repo must be creatable");
    let repo_path = temp.path().to_path_buf();
    git(&["init"], &repo_path);
    git(&["config", "user.name", "Ironmem Test"], &repo_path);
    git(&["config", "user.email", "ironmem@example.com"], &repo_path);
    let head = commit_file(&repo_path, "a.txt", "one\n", "first commit");
    let path = repo_path.to_string_lossy().to_string();
    (temp, path, head)
}

#[test]
fn collab_checkpoint_persists_and_is_readable_back() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let written = call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "task_id": 3,
            "task_title": "Add the gate",
            "status": "completed",
            "head_sha": &head,
            "commit_sha": &head,
            "completed_task_ids": "1,2,3",
            "next_task_id": 4,
            "gates_result": "passed",
            "gates_sha": &head,
            "gates_commands": "cargo test --workspace",
            "summary": "task 3 done"
        }),
    );
    assert_eq!(written["session_id"], session_id);
    assert_eq!(written["status"], "completed");
    assert_eq!(written["head_sha"], head);

    let stored = stored_checkpoint(&app, &session_id).expect("checkpoint row must exist");
    assert_eq!(stored.status, ironmem::collab::CheckpointStatus::Completed);
    assert_eq!(stored.head_sha, head);
    assert_eq!(stored.task_id, Some(3));
    assert_eq!(stored.completed_task_ids, vec![1, 2, 3]);
    assert_eq!(stored.next_task_id, Some(4));
    assert_eq!(stored.gates_result, "passed");
    assert_eq!(stored.attested_by, ironmem::collab::AttestedBy::Implementer);
    assert!(
        stored.updated_at > 0,
        "the server must stamp updated_at at write time, got {}",
        stored.updated_at
    );
    // Echoed back, and echoed as what the ROW says: a caller checking that its
    // checkpoint landed fresh must be reading the server's stamp, not a value
    // the response computed on its own.
    assert_eq!(
        written["updated_at"],
        json!(stored.updated_at),
        "the response must echo the stamp the row carries"
    );
    assert_eq!(written["agent"], "claude");

    let (params, result) = last_wal_row(&app, "collab_checkpoint");
    assert_eq!(params["session_id"], session_id);
    assert_eq!(params["agent"], "claude");
    assert_eq!(params["head_sha"], head);
    assert_eq!(params["attested_by"], "implementer");
    assert_eq!(result["completed_task_ids"], json!([1, 2, 3]));
}

/// A second checkpoint replaces the first: the contract is exactly one
/// *current* checkpoint per session, so a stale row must never survive
/// alongside a fresher one.
#[test]
fn collab_checkpoint_overwrites_the_previous_checkpoint() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "task_id": 1,
            "status": "started",
            "head_sha": "b9c2ce0"
        }),
    );
    call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "batch_complete",
            "head_sha": &head,
            "completed_task_ids": "1,2"
        }),
    );

    let stored = stored_checkpoint(&app, &session_id).expect("checkpoint row must exist");
    assert_eq!(
        stored.status,
        ironmem::collab::CheckpointStatus::BatchComplete
    );
    assert_eq!(stored.head_sha, head);
    assert_eq!(stored.completed_task_ids, vec![1, 2]);
    assert_eq!(stored.task_id, None);
}

/// `CollabCheckpoint::from_json`'s rejection must reach the caller as a
/// validation error naming the field — not as a raw SQL CHECK violation from
/// migration 020, and not as a silently-accepted write.
#[test]
fn collab_checkpoint_rejects_an_unknown_status() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let err = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "nearly_done",
            "head_sha": &head
        }),
    );
    assert!(
        err.contains("status") && err.contains("nearly_done"),
        "an unknown status must surface as a validation error naming the field \
         and the offending value: {err}"
    );
    // `tool_error_response` renders `MemoryError::Db` as the opaque "Internal
    // server error" and only a `Validation`/`NotFound`/`Permission` message
    // verbatim. So this is the assertion that the *parser* rejected: had the
    // payload reached migration 020's `CHECK (status IN (...))` instead, the
    // caller would be told nothing at all.
    assert_ne!(
        err, "Internal server error",
        "a bad status must not surface as a raw SQL error"
    );
    assert!(
        stored_checkpoint(&app, &session_id).is_none(),
        "a rejected checkpoint must persist nothing"
    );
    assert_eq!(
        wal_row_count(&app, &session_id, "collab_checkpoint"),
        0,
        "a rejected checkpoint must write no audit row"
    );
}

/// The operator-attestation rule (D1): only a human may vouch for commits the
/// protocol never witnessed, so an implementer-attested payload carrying
/// `acknowledged_divergence` is refused outright.
#[test]
fn collab_checkpoint_rejects_an_implementer_attested_divergence() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let err = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "batch_complete",
            "head_sha": &head,
            "acknowledged_divergence": "b9c2ce0..75a4ea3"
        }),
    );
    assert!(
        err.contains("acknowledged_divergence") && err.contains("implementer"),
        "an implementer self-attestation over a divergence must be refused with \
         a validation message naming the rule: {err}"
    );
    // As above: migration 020's one-directional CHECK also refuses this row,
    // but as an opaque "Internal server error". The named message is the proof
    // the parser got there first.
    assert_ne!(
        err, "Internal server error",
        "the rule must be enforced in Rust, not only by the schema"
    );
    assert!(
        stored_checkpoint(&app, &session_id).is_none(),
        "a rejected checkpoint must persist nothing"
    );
}

/// The escape hatch, end to end: an operator naming the range they vouch for
/// may checkpoint over a divergence. The write is still *reported* as diverged
/// — reporting is not refusing — because a checkpoint write is how drift gets
/// fixed, and refusing on drift would make the recovery path unreachable.
#[test]
fn collab_checkpoint_accepts_an_operator_attested_divergence() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let written = call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "batch_complete",
            "head_sha": "b9c2ce0",
            "attested_by": "operator",
            "acknowledged_divergence": format!("b9c2ce0..{head}")
        }),
    );
    assert_eq!(written["diverged"], json!(true));

    let stored = stored_checkpoint(&app, &session_id).expect("checkpoint row must exist");
    assert_eq!(stored.attested_by, ironmem::collab::AttestedBy::Operator);
    assert_eq!(
        stored.acknowledged_divergence.as_deref(),
        Some(format!("b9c2ce0..{head}").as_str())
    );
}

/// The happy case for the head check: live HEAD equals the checkpoint's
/// `head_sha`, so the answer is a *verified* "no drift" — `head_check` says the
/// check ran, and `repo_head_sha` names what it read.
#[test]
fn collab_checkpoint_reports_a_verified_match_against_live_head() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let written = call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "completed",
            "task_id": 1,
            "head_sha": &head
        }),
    );
    assert_eq!(written["diverged"], json!(false));
    assert_eq!(written["head_check"], "checked");
    assert_eq!(written["repo_head_sha"], head);
}

/// Issue #273 itself: the checkpoint names an older SHA than the branch has.
/// The write must still land — this is how an operator files an accurate
/// checkpoint — while the response says so.
#[test]
fn collab_checkpoint_reports_divergence_without_refusing_the_write() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let written = call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "started",
            "task_id": 1,
            "head_sha": "b9c2ce0"
        }),
    );
    assert_eq!(written["diverged"], json!(true));
    assert_eq!(written["head_check"], "checked");
    assert_eq!(
        written["repo_head_sha"], head,
        "the response must name the HEAD it actually read, so the caller can \
         file an accurate checkpoint"
    );
    assert_eq!(
        stored_checkpoint(&app, &session_id)
            .expect("a diverged checkpoint must still be written")
            .head_sha,
        "b9c2ce0"
    );

    let (_params, result) = last_wal_row(&app, "collab_checkpoint");
    assert_eq!(result["diverged"], json!(true));
}

/// The constraint-1 case. When git cannot be read at all, "no drift" is not a
/// finding — it is an unrun check. Reporting `diverged: false` there would
/// present an unverified claim as verified, which is the exact failure #273
/// exists to end, so the third state is reported as such.
#[test]
fn collab_checkpoint_does_not_report_unverified_head_as_undiverged() {
    let app = App::open_for_test().unwrap();
    // A path that is not a git repo (and does not exist), so `git rev-parse`
    // fails: the "could not check" case, not the "no drift" case.
    let session_id = start_checkpoint_session(&app, "/nonexistent/ironmem-repo", "main");

    let written = call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "started",
            "task_id": 1,
            "head_sha": "b9c2ce0"
        }),
    );
    assert_eq!(
        written["diverged"],
        json!(null),
        "an unrun check must not answer false: {written}"
    );
    assert_eq!(written["head_check"], "unreadable");
    assert_eq!(written["repo_head_sha"], json!(null));
    assert!(
        written["head_check_error"]
            .as_str()
            .is_some_and(|detail| detail.contains("git")),
        "the caller must be told why the check could not run: {written}"
    );
    assert!(
        stored_checkpoint(&app, &session_id).is_some(),
        "an unreadable repo must not block the checkpoint write"
    );

    // The audit trail must not record the unverified claim as verified either.
    let (_params, result) = last_wal_row(&app, "collab_checkpoint");
    assert_eq!(result["diverged"], json!(null));
    assert_eq!(result["head_check"], "unreadable");
}

/// The tool has exactly ONE session id: the parsed, trimmed one. It addresses
/// the session-existence check, keys the row, and is echoed back — so the
/// session whose liveness was checked is always the session the row is written
/// under. A padded value is the case that pulls those apart if a second,
/// unparsed reading of the argument ever creeps back in: the existence check
/// would miss (`NotFound`) or the row would be keyed by a string no session
/// has, which migration 020's foreign key refuses.
#[test]
fn collab_checkpoint_uses_one_session_id_for_lookup_and_row() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let written = call_tool(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": format!("  {session_id}  "),
            "agent": "claude",
            "status": "started",
            "task_id": 1,
            "head_sha": &head
        }),
    );
    assert_eq!(written["session_id"], session_id);
    assert!(
        stored_checkpoint(&app, &session_id).is_some(),
        "the row must be keyed by the same session id the existence check used"
    );
}

/// The payload parser is the ONLY reader of `session_id` here, which is what
/// lets the diagnosis survive: it can tell a wrong-typed value from an absent
/// one. `shared::require_str`, the helper the neighbouring handlers reach for,
/// collapses both into "session_id is required" — true but useless to a caller
/// that sent a number and needs to be told so.
#[test]
fn collab_checkpoint_names_a_wrong_typed_session_id() {
    let app = App::open_for_test().unwrap();

    let err = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({ "session_id": 42, "agent": "claude", "status": "started", "head_sha": "b9c2ce0" }),
    );
    assert_eq!(err, "session_id must be a string", "got: {err}");

    let absent = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({ "agent": "claude", "status": "started", "head_sha": "b9c2ce0" }),
    );
    assert_eq!(
        absent, "session_id is required and must be a non-empty string",
        "got: {absent}"
    );
}

/// The generation lease, driven across two `App` instances over one on-disk
/// DB — the real deployment topology, where each agent runs its own `ironmem`
/// MCP server process (see `open_second_disk_app`).
///
/// A superseded process must not be able to land its stale view of progress.
/// Nothing downstream could catch it if it did: `updated_at` is server-stamped,
/// so the stale content would arrive carrying a *fresh* timestamp, and a Task 7
/// gate asking "is this checkpoint recent?" would be answered by the very
/// anti-backdating stamp that exists to prevent this. Hence the check at the
/// write.
///
/// The final assertion is on the stored ROW, not on the error string: an
/// error message proves the call was answered, only the row proves nothing
/// was written.
#[test]
fn collab_checkpoint_refuses_a_superseded_process() {
    let (_dir_a, db_path, app_a) = open_disk_app_for_dashboard_sweep();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app_a, &repo_path, "main");

    // The incumbent process files a checkpoint tokenlessly, binding itself to
    // the session's current generation.
    call_tool(
        &app_a,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "task_id": 1,
            "status": "started",
            "head_sha": &head
        }),
    );

    // A successor process takes the session over: mint a handoff token and
    // spend it on its own checkpoint, which advances claude's generation.
    let (_dir_b, app_b) = open_second_disk_app(&db_path);
    let issued = call_tool(
        &app_b,
        "session_handoff",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    let token = issued["handoff_token"]
        .as_str()
        .expect("session_handoff must return a token")
        .to_string();
    let successor = call_tool(
        &app_b,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "status": "batch_complete",
            "head_sha": &head,
            "completed_task_ids": "1,2,3",
            "handoff_token": &token
        }),
    );
    assert_eq!(successor["status"], "batch_complete");

    // The superseded process now tries to file its stale "task 1 / started"
    // progress — the issue #273 shape, one process behind.
    let err = call_tool_expect_error(
        &app_a,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "claude",
            "task_id": 1,
            "status": "started",
            "head_sha": &head
        }),
    );
    assert!(
        err.contains("stale collab generation"),
        "the superseded process must be refused by the generation lease: {err}"
    );

    let stored = stored_checkpoint(&app_b, &session_id).expect("checkpoint row must exist");
    assert_eq!(
        stored.status,
        ironmem::collab::CheckpointStatus::BatchComplete,
        "the successor's progress must survive the superseded process's write"
    );
    assert_eq!(stored.completed_task_ids, vec![1, 2, 3]);
    assert_eq!(stored.task_id, None);
}

/// `agent` is required, not optional-and-checked-when-present: a superseded
/// process would simply omit an optional one, and an authorization check the
/// caller can decline is not a check. Pinned separately from the lease test
/// because the lease is only reachable once an agent has been named.
#[test]
fn collab_checkpoint_requires_an_agent() {
    let app = App::open_for_test().unwrap();
    let (_repo, repo_path, head) = checkpoint_repo();
    let session_id = start_checkpoint_session(&app, &repo_path, "main");

    let err = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({ "session_id": &session_id, "status": "started", "head_sha": &head }),
    );
    assert_eq!(err, "agent is required", "got: {err}");
    assert!(
        stored_checkpoint(&app, &session_id).is_none(),
        "an unauthenticated checkpoint must persist nothing"
    );

    let bad = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({
            "session_id": &session_id,
            "agent": "operator",
            "status": "started",
            "head_sha": &head
        }),
    );
    assert_eq!(bad, "agent must be 'claude' or 'codex'", "got: {bad}");
    assert!(stored_checkpoint(&app, &session_id).is_none());
}

/// A checkpoint is session-scoped state, so it needs a live session: an
/// unknown id is a `NotFound`, and an ended one a validation error. Without
/// this the FK would still refuse the unknown id, but as a raw SQL error.
#[test]
fn collab_checkpoint_requires_a_live_session() {
    let app = App::open_for_test().unwrap();
    let head = "b9c2ce0";

    let unknown = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({ "session_id": "no-such-session", "agent": "claude", "status": "started", "head_sha": head }),
    );
    assert!(
        unknown.contains("not found"),
        "an unknown session must be a NotFound: {unknown}"
    );

    // `collab_end` is only legal from a few phases, so this one is driven to
    // PlanLocked first.
    let session_id = drive_to_plan_locked(&app, "final plan text");
    call_tool(
        &app,
        "collab_end",
        json!({ "session_id": &session_id, "agent": "claude" }),
    );
    let ended = call_tool_expect_error(
        &app,
        "collab_checkpoint",
        json!({ "session_id": &session_id, "agent": "claude", "status": "started", "head_sha": head }),
    );
    assert!(
        ended.contains("has ended"),
        "an ended session must be refused: {ended}"
    );
    assert!(
        stored_checkpoint(&app, &session_id).is_none(),
        "a refused checkpoint must persist nothing"
    );
}

// ── implementation_done checkpoint gate (issue #273 Task 7) ─────────────────
//
// The acceptance criterion for the whole issue: a runner cannot report a
// normal batch state when the checkpoint does not back the claim. Every
// refusal test below asserts the *stored phase* as well as the error — an
// error proves the call was answered; only the row proves the session did not
// advance anyway.

/// The head sha every gate test reports on `implementation_done`.
///
/// A real, deterministically-reproducible commit sha (issue #273 Task 8), not
/// an arbitrary string: [`gate_head_repo`] below produces exactly this value
/// by committing fixed content under a fixed author/committer identity and
/// timestamp, so it is always both a well-formed 40-hex sha AND a real object
/// in the repo `drive_to_code_implement_pending` seeds every gate test with.
/// Task 8 added git-ancestry validation to `implementation_done`; an
/// arbitrary made-up sha (this constant's value before Task 8) would now be
/// refused as `branch_drift:` before ever reaching Task 7's checkpoint gate —
/// which is not what any test in this section means to exercise.
const GATE_HEAD: &str = "fca655866ba97de53fb6a0029a1f65804a78f903";

/// Commit with a fixed author/committer identity and timestamp so the
/// resulting sha is reproducible across machines and runs. Required for
/// [`GATE_HEAD`] to be a fixed literal that is also always a real, reachable
/// commit — a plain `commit_file` call would produce a different sha on every
/// run (committer date defaults to "now").
fn commit_file_deterministic(
    cwd: &Path,
    filename: &str,
    contents: &str,
    message: &str,
    date: &str,
) -> String {
    write_file(&cwd.join(filename), contents);
    git(&["add", filename], cwd);
    let mut command = Command::new("git");
    // Scrub every inherited `GIT_*` var first — same reason `git()` does —
    // before setting the specific author/committer ones below. An inherited
    // `GIT_DIR` would silently redirect this commit (and the determinism
    // `GATE_HEAD` depends on) to a different repo than `cwd`.
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
        .args(["commit", "-m", message])
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Ironmem Test")
        .env("GIT_AUTHOR_EMAIL", "ironmem@example.com")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_NAME", "Ironmem Test")
        .env("GIT_COMMITTER_EMAIL", "ironmem@example.com")
        .env("GIT_COMMITTER_DATE", date);
    let output = command.output().expect("git commit must run");
    assert!(
        output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    git(&["rev-parse", "HEAD"], cwd)
}

/// A repo containing the real, deterministic two-commit chain that produces
/// [`GATE_HEAD`]. `commit.gpgsign` and `core.hooksPath` are pinned off for the
/// same reason every other git fixture in this file pins them off: an
/// inherited signing key or personal hooksPath would make this fragile
/// outside the machine that happens to have them configured.
fn gate_head_repo() -> (tempfile::TempDir, PathBuf, String) {
    let temp = tempfile::tempdir().expect("temp repo must be creatable");
    let repo_path = temp.path().to_path_buf();
    git(&["init"], &repo_path);
    git(&["config", "user.name", "Ironmem Test"], &repo_path);
    git(&["config", "user.email", "ironmem@example.com"], &repo_path);
    git(&["config", "commit.gpgsign", "false"], &repo_path);
    git(&["config", "core.hooksPath", "/dev/null"], &repo_path);
    let base_sha = commit_file_deterministic(
        &repo_path,
        "gate.txt",
        "base",
        "gate base",
        "2020-01-01T00:00:00+00:00",
    );
    let head_sha = commit_file_deterministic(
        &repo_path,
        "gate.txt",
        "head",
        "gate head",
        "2020-01-01T00:01:00+00:00",
    );
    assert_eq!(
        head_sha, GATE_HEAD,
        "the deterministic gate-head recipe drifted from the pinned GATE_HEAD constant — if \
         this fires, either the recipe above was edited without recomputing GATE_HEAD, or git's \
         commit-hashing changed shape"
    );
    (temp, repo_path, base_sha)
}

/// Drive a fresh session to `CodeImplementPending` with `tasks` tasks, seeded
/// in [`gate_head_repo`] so `GATE_HEAD` is a real descendant of the session's
/// `last_head_sha`. Returns the `TempDir` alongside the session id — the
/// caller must keep it alive for the session's lifetime, since dropping it
/// deletes the repo `implementation_done`'s ancestry check later shells out
/// to.
fn drive_to_code_implement_pending(app: &App, tasks: usize) -> (tempfile::TempDir, String) {
    let (temp, repo_path, base_sha) = gate_head_repo();
    let session_id = drive_to_plan_locked_in_repo(app, "gate plan", &repo_path);
    let hash = plan_hash(app, &session_id);
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, "b0", &base_sha, tasks)
        }),
    );
    let status = call_tool(app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeImplementPending");
    (temp, session_id)
}

/// A checkpoint payload that satisfies *every* condition of the gate, so each
/// test can break exactly one of them and know which condition it is
/// exercising. Deliberately built as a full payload rather than by mutating a
/// previous write: the conditions must be checked independently, and a shared
/// mutated fixture is how one of them ends up masking another.
fn passing_checkpoint(session_id: &str, tasks: u64) -> serde_json::Value {
    batch_complete_checkpoint(session_id, "claude", GATE_HEAD, tasks)
}

/// Attempt the gated send and return the refusal text, having first proved
/// the session did not advance.
fn implementation_done_refused(app: &App, session_id: &str) -> String {
    let sends_before = wal_row_count(app, session_id, "collab_send");
    let err = call_tool_expect_error(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": GATE_HEAD }).to_string()
        }),
    );
    // The gate sits before `apply_event`, so the refusal must roll back the
    // whole turn — no audit row, no queued message, no phase move. Counting
    // the audit rows is what distinguishes "refused" from "refused after
    // recording that it happened".
    assert_eq!(
        wal_row_count(app, session_id, "collab_send"),
        sends_before,
        "a refused implementation_done must write no audit row: {err}"
    );
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(
        status["phase"], "CodeImplementPending",
        "a refused implementation_done must leave the session in CodeImplementPending, \
         not merely return an error: {err}"
    );
    assert_eq!(
        status["current_owner"], "claude",
        "a refused implementation_done must not hand the turn on: {err}"
    );
    assert_ne!(
        status["last_head_sha"],
        json!(GATE_HEAD),
        "a refused implementation_done must not record the reported head: {err}"
    );
    // The prefix is what makes the condition reportable off-turn as a
    // recoverable failure, so it is asserted on every refusal rather than
    // only on the ones whose wording a test happens to inspect. Matched with
    // `contains` because `MemoryError::Validation`'s own "Validation error:"
    // rendering sits in front of it on the wire.
    assert!(
        err.contains("checkpoint_drift:"),
        "every gate refusal must carry the checkpoint_drift: prefix, got: {err}"
    );
    err
}

/// Pull one `<key>=<value>` argument out of the `collab_checkpoint(...)`
/// remedy embedded in a refusal. Quoted values end at the closing quote, bare
/// ones at the next `,` or the closing `)`.
fn remedy_field(err: &str, key: &str) -> String {
    let needle = format!("{key}=");
    let start = err
        .find(&needle)
        .unwrap_or_else(|| panic!("the remedy names no {key}: {err}"))
        + needle.len();
    let rest = &err[start..];
    match rest.strip_prefix('"') {
        Some(quoted) => {
            let end = quoted
                .find('"')
                .unwrap_or_else(|| panic!("unterminated quoted {key}: {err}"));
            quoted[..end].to_string()
        }
        None => {
            let end = rest
                .find([',', ')'])
                .unwrap_or_else(|| panic!("unterminated {key}: {err}"));
            rest[..end].to_string()
        }
    }
}

/// Do exactly what the refusal told the caller to do: send a
/// `collab_checkpoint` built from the arguments the remedy named, taking
/// nothing from the test's own knowledge of the session.
///
/// `agent=<you>` is the one argument the remedy deliberately leaves as a
/// placeholder — it names the caller's own identity, which the server cannot
/// fill in — so it is the one value substituted here.
fn follow_the_remedy(app: &App, err: &str) {
    let completed = remedy_field(err, "completed_task_ids");
    // A property, not a literal: any future ellipsis, range dash, or prose
    // creeps back in as a non-digit. `parse_completed_task_ids` rejects those,
    // and the tool call below is what proves it — this assertion just fails
    // with a message that says which character was wrong.
    assert!(
        completed.chars().all(|c| c.is_ascii_digit() || c == ','),
        "the remedy's completed_task_ids must be a literal comma-separated list \
         the server can parse, got {completed:?} in: {err}"
    );
    call_tool(
        app,
        "collab_checkpoint",
        json!({
            "session_id": remedy_field(err, "session_id"),
            "agent": "claude",
            "status": remedy_field(err, "status"),
            "head_sha": remedy_field(err, "head_sha"),
            "completed_task_ids": completed,
            "gates_result": remedy_field(err, "gates_result"),
            "gates_sha": remedy_field(err, "gates_sha"),
        }),
    );
}

/// The refusal's remedy is a *machine-followable* instruction — an agent that
/// hits this gate is expected to copy it verbatim — so follow it verbatim and
/// require the retried send to be accepted.
///
/// Deliberately not a string match on the rendered list. Asserting
/// `completed_task_ids="1,2,3"` would still pass if the server's own parser
/// disagreed with that format; feeding the emitted value back through the real
/// `collab_checkpoint` tool is what proves the advice works. It would have
/// caught the ellipsis form `1,..,3`, which reads as an obvious range to a
/// human and is a parse error to `parse_completed_task_ids` — an agent
/// following it would earn a second, unrelated error and another trip round
/// the recovery loop this gate exists to open.
///
/// Run from two different conditions because all four refusals embed the same
/// remedy: one arriving with no checkpoint at all, one with a checkpoint whose
/// ledger under-covers.
#[test]
fn the_refusal_remedy_is_a_call_that_actually_satisfies_the_gate() {
    for under_covering_checkpoint in [false, true] {
        let app = App::open_for_test().unwrap();
        let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);
        if under_covering_checkpoint {
            let mut cp = passing_checkpoint(&session_id, 3);
            cp["completed_task_ids"] = json!("1,2");
            call_tool(&app, "collab_checkpoint", cp);
        }

        let err = implementation_done_refused(&app, &session_id);
        assert_eq!(
            remedy_field(&err, "head_sha"),
            GATE_HEAD,
            "the remedy must point at the head being reported, not the stale one: {err}"
        );

        follow_the_remedy(&app, &err);

        call_tool(
            &app,
            "collab_send",
            json!({
                "session_id": &session_id,
                "sender": "claude",
                "topic": "implementation_done",
                "content": json!({ "head_sha": GATE_HEAD }).to_string()
            }),
        );
        let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
        assert_eq!(
            status["phase"], "CodeReviewFixGlobalPending",
            "following the remedy verbatim must satisfy the gate \
             (under_covering_checkpoint={under_covering_checkpoint})"
        );
    }
}

/// Condition 1: a session that never checkpointed has no progress claim to
/// verify, and must be told which tool fixes that. Waving legacy/never-written
/// sessions through would reinstate exactly the hole issue #273 closes.
#[test]
fn implementation_done_refused_without_any_checkpoint() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);

    let err = implementation_done_refused(&app, &session_id);
    assert!(
        err.contains("collab_checkpoint"),
        "the refusal must name the tool that fixes it, got: {err}"
    );
    assert!(
        stored_checkpoint(&app, &session_id).is_none(),
        "the refusal must not fabricate a checkpoint"
    );
}

/// Condition 2, and the incident itself: 28 commits landed while the
/// checkpoint stayed frozen at task 1's head.
#[test]
fn implementation_done_refused_when_the_checkpoint_head_is_stale() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);
    let mut cp = passing_checkpoint(&session_id, 3);
    cp["head_sha"] = json!("b9c2ce0e1d2c3b4a5968778695a4b3c2d1e0f9a8");
    cp["gates_sha"] = json!("b9c2ce0e1d2c3b4a5968778695a4b3c2d1e0f9a8");
    call_tool(&app, "collab_checkpoint", cp);

    let err = implementation_done_refused(&app, &session_id);
    assert!(
        err.contains("b9c2ce0") && err.contains(GATE_HEAD),
        "the refusal must name both shas so the operator can see the drift, got: {err}"
    );
}

/// Condition 2's diagnosability requirement: the comparison is raw string
/// equality, so an abbreviated sha reads as permanent drift. An operator
/// staring at two shas that look identical must be told why they are not.
#[test]
fn implementation_done_refusal_explains_an_abbreviated_sha() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 1);
    let short = &GATE_HEAD[..7];
    let mut cp = passing_checkpoint(&session_id, 1);
    cp["head_sha"] = json!(short);
    cp["gates_sha"] = json!(short);
    call_tool(&app, "collab_checkpoint", cp);

    let err = implementation_done_refused(&app, &session_id);
    assert!(
        err.contains("abbreviated") || err.contains("prefix"),
        "a short sha must be diagnosed, not left as two lookalike strings, got: {err}"
    );
    assert!(
        err.contains("7 chars") && err.contains("40 chars"),
        "the refusal must show the lengths that differ, got: {err}"
    );
}

/// Condition 3a: `batch_complete` is the only status that claims the batch is
/// finished. The incident's checkpoint said `started`.
#[test]
fn implementation_done_refused_when_the_checkpoint_is_not_batch_complete() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);
    let mut cp = passing_checkpoint(&session_id, 3);
    cp["status"] = json!("completed");
    call_tool(&app, "collab_checkpoint", cp);

    let err = implementation_done_refused(&app, &session_id);
    assert!(
        err.contains("batch_complete") && err.contains("completed"),
        "the refusal must name the required status and the one recorded, got: {err}"
    );
}

/// Condition 3b: reporting the batch done while the ledger shows 2 of 3 is a
/// false progress report even when the shas agree.
#[test]
fn implementation_done_refused_when_the_checkpoint_misses_a_task() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);
    let mut cp = passing_checkpoint(&session_id, 3);
    cp["completed_task_ids"] = json!("1,2");
    call_tool(&app, "collab_checkpoint", cp);

    let err = implementation_done_refused(&app, &session_id);
    // `contains("of the 3")` rather than `contains('3')`: the session id is a
    // UUID, so a bare `'3'` is very nearly always present and would assert
    // almost nothing about the message.
    assert!(
        err.contains("1, 2") && err.contains("of the 3"),
        "the refusal must name what is covered and how many tasks there are, got: {err}"
    );
}

/// Coverage is set membership, not arithmetic: `1,2,4` over three tasks has
/// the right count and the wrong contents. Pinned separately from the
/// missing-task case because a length-only gate passes that one.
#[test]
fn implementation_done_refused_when_the_covered_ids_have_a_gap() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);
    let mut cp = passing_checkpoint(&session_id, 3);
    cp["completed_task_ids"] = json!("1,2,4");
    call_tool(&app, "collab_checkpoint", cp);

    implementation_done_refused(&app, &session_id);
}

/// Condition 4: green gates at an older sha describe a tree that no longer
/// exists.
#[test]
fn implementation_done_refused_when_the_gate_proof_is_stale() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 2);
    let mut cp = passing_checkpoint(&session_id, 2);
    cp["gates_sha"] = json!("older99e1d2c3b4a5968778695a4b3c2d1e0f9a8b");
    call_tool(&app, "collab_checkpoint", cp);

    let err = implementation_done_refused(&app, &session_id);
    assert!(
        err.contains("gates"),
        "the refusal must name the gate proof, got: {err}"
    );
}

/// Condition 4's other half: gates that never ran, or ran red, are not a
/// proof at all even when the shas line up.
#[test]
fn implementation_done_refused_when_the_gates_did_not_pass() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 2);
    let mut cp = passing_checkpoint(&session_id, 2);
    cp["gates_result"] = json!("failed: 3 tests red");
    call_tool(&app, "collab_checkpoint", cp);

    implementation_done_refused(&app, &session_id);
}

/// The positive direction. A gate that refuses everything is not a gate, and
/// this is the case that proves the four conditions are jointly satisfiable
/// by an honest implementer.
#[test]
fn implementation_done_accepted_with_a_checkpoint_that_proves_the_batch() {
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 3);
    call_tool(
        &app,
        "collab_checkpoint",
        passing_checkpoint(&session_id, 3),
    );

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": GATE_HEAD }).to_string()
        }),
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["last_head_sha"], GATE_HEAD);
}

/// The gate does not consult `attested_by`, in either direction.
///
/// It never compares the checkpoint against *live* git HEAD — only against the
/// head the caller is reporting in this same payload — and a "divergence" is
/// by definition a checkpoint-vs-live-HEAD disagreement. So an operator
/// attestation has nothing to excuse here: it neither exempts a checkpoint
/// from the four conditions (which would make the gate bypassable by setting
/// one field) nor is refused by them (which would leave Task 10's escape hatch
/// with nothing to build on). Both halves are asserted.
#[test]
fn implementation_done_gate_ignores_the_operator_attestation() {
    // Half 1: an operator attestation does NOT exempt a stale checkpoint.
    let app = App::open_for_test().unwrap();
    let (_temp, session_id) = drive_to_code_implement_pending(&app, 2);
    let mut cp = passing_checkpoint(&session_id, 2);
    cp["head_sha"] = json!("b9c2ce0e1d2c3b4a5968778695a4b3c2d1e0f9a8");
    cp["gates_sha"] = json!("b9c2ce0e1d2c3b4a5968778695a4b3c2d1e0f9a8");
    cp["attested_by"] = json!("operator");
    cp["acknowledged_divergence"] = json!("b9c2ce0..75a4ea3");
    call_tool(&app, "collab_checkpoint", cp);
    implementation_done_refused(&app, &session_id);

    // Half 2: an operator-attested checkpoint that DOES satisfy the four
    // conditions passes — the gate refuses nothing on the strength of
    // `attested_by` alone, so Task 10 has a reachable path to extend.
    let app2 = App::open_for_test().unwrap();
    let (_temp2, session2) = drive_to_code_implement_pending(&app2, 2);
    let mut ok = passing_checkpoint(&session2, 2);
    ok["attested_by"] = json!("operator");
    ok["acknowledged_divergence"] = json!("b9c2ce0..75a4ea3");
    call_tool(&app2, "collab_checkpoint", ok);
    call_tool(
        &app2,
        "collab_send",
        json!({
            "session_id": &session2,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": GATE_HEAD }).to_string()
        }),
    );
    let status = call_tool(&app2, "collab_status", json!({ "session_id": &session2 }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
}

/// The gate is scoped to `implementation_done`. Every other coding topic must
/// still be sendable without a checkpoint, or a checkpoint-less session that
/// legitimately reached global review could never finish.
#[test]
fn the_checkpoint_gate_does_not_apply_to_the_review_topics() {
    let app = App::open_for_test().unwrap();
    let (temp, session_id) = drive_to_code_implement_pending(&app, 1);
    call_tool(
        &app,
        "collab_checkpoint",
        passing_checkpoint(&session_id, 1),
    );
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": GATE_HEAD }).to_string()
        }),
    );

    // Real repo (issue #273 Task 8): the review phases' heads are now
    // git-ancestry-checked too, so "later-head" must be a real descendant of
    // GATE_HEAD — add one more commit to the same repo `temp` still owns.
    let later_head = commit_file(temp.path(), "gate.txt", "later", "later head");

    // The checkpoint is now stale relative to every later head, which must
    // not stop the review phases from running.
    for (sender, topic, extra) in [
        ("codex", "review_fix_global", json!({})),
        ("claude", "review_local", json!({})),
        (
            "claude",
            "final_review",
            json!({ "pr_url": "https://example.test/pr/1" }),
        ),
    ] {
        let mut content = json!({ "head_sha": &later_head });
        for (k, v) in extra.as_object().unwrap() {
            content[k] = v.clone();
        }
        call_tool(
            &app,
            "collab_send",
            json!({
                "session_id": &session_id,
                "sender": sender,
                "topic": topic,
                "content": content.to_string()
            }),
        );
    }
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "CodingComplete");
}

// ── ancestry validation extended to the v3 batch flow (issue #273 Task 8) ──
//
// Task 7's `implementation_done` gate proves the checkpoint's own story is
// self-consistent: the reported head matches what the checkpoint claims, the
// ledger covers every task, gates are green at that head. It does not prove
// the reported head is *real* — a caller can file a checkpoint at a head_sha
// it invented and report that same invented value back, and every one of
// Task 7's four conditions is satisfied by construction. Only asking git
// whether the reported head actually descends from the session's last
// recorded head closes that gap, which is what production code now does for
// every head-advancing coding event, not just the `collab_start_code_review`
// shortcut it used to be limited to.

/// A fresh git repo, isolated from this machine's global git config —
/// `commit.gpgsign` and `core.hooksPath` are pinned off explicitly rather
/// than left to whatever the developer machine running this test happens to
/// have configured. A working SSH/GPG signing key locally makes an inherited
/// `commit.gpgsign=true` invisible here and a silent hang-or-fail on a CI
/// runner with no key configured; an inherited `core.hooksPath` risks running
/// someone's personal hooks against a throwaway fixture repo — with `n`
/// sequential commits, each a real descendant of the one before.
fn git_batch_repo(n: usize) -> (tempfile::TempDir, PathBuf, Vec<String>) {
    let temp = tempfile::tempdir().expect("temp repo must be creatable");
    let repo_path = temp.path().to_path_buf();
    git(&["init"], &repo_path);
    git(&["config", "user.name", "Ironmem Test"], &repo_path);
    git(&["config", "user.email", "ironmem@example.com"], &repo_path);
    git(&["config", "commit.gpgsign", "false"], &repo_path);
    git(&["config", "core.hooksPath", "/dev/null"], &repo_path);
    let shas = (0..n)
        .map(|i| {
            commit_file(
                &repo_path,
                "batch.txt",
                &format!("v{i}\n"),
                &format!("commit {i}"),
            )
        })
        .collect();
    (temp, repo_path, shas)
}

/// An `App` paired with a fresh [`git_batch_repo`]. Named to match what issue
/// #273's Task 9 plans to build as a shared fixture (`test_app_with_git_repo`)
/// — adopt the *name* rather than duplicating it, but note the shape does not
/// match Task 9's plan as written: the plan calls
/// `let (app, _tmp, repo) = test_app_with_git_repo();` (no argument, a
/// 3-tuple with no commits made yet), while this is
/// `test_app_with_git_repo(n_commits) -> (App, TempDir, PathBuf, Vec<String>)`
/// — a commit count in, and the resulting shas out, because every caller in
/// this file needs at least one real commit before it can drive a session
/// anywhere. Task 9 should treat this as a starting point to reconcile
/// against its own plan, not a fixture it can call unmodified.
fn test_app_with_git_repo(n_commits: usize) -> (App, tempfile::TempDir, PathBuf, Vec<String>) {
    let app = App::open_for_test().unwrap();
    let (temp, repo_path, shas) = git_batch_repo(n_commits);
    (app, temp, repo_path, shas)
}

/// `collab_status`'s `phase` field, as an owned `String`. Named to match what
/// issue #273's Task 9 plans to build (`phase_of`) — adopt this rather than
/// duplicating it.
fn phase_of(app: &App, session_id: &str) -> String {
    call_tool(app, "collab_status", json!({ "session_id": session_id }))["phase"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The core case: the batch flow now refuses a non-descendant `head_sha` on
/// `implementation_done`, exactly as the shortcut already refused it on
/// `review_fix_global`/`review_local`.
///
/// The checkpoint filed here is a **passing** one — it satisfies every Task 7
/// condition at `orphan_sha`, the exact "checkpoint lies consistently"
/// scenario Task 8 exists to close. If this test filed no checkpoint (or an
/// under-covering one) instead, it would still get refused, but by Task 7's
/// gate rather than Task 8's — proving nothing about the ancestry check this
/// test exists to pin. An orphan commit (`git checkout --orphan`) is the
/// cleanest possible non-descendant: it shares no parent with `base_sha` at
/// all, so there is no ambiguity with "on the same branch but not far
/// enough".
#[test]
fn batch_flow_implementation_done_rejects_non_descendant_head() {
    let (app, _temp, repo_path, shas) = test_app_with_git_repo(1);
    let base_sha = shas[0].clone();
    let session_id = drive_to_plan_locked_in_repo(&app, "batch plan", &repo_path);
    let hash = plan_hash(&app, &session_id);
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, &base_sha, &base_sha, 1)
        }),
    );
    assert_eq!(phase_of(&app, &session_id), "CodeImplementPending");

    git(&["checkout", "--orphan", "unrelated"], &repo_path);
    let orphan_sha = commit_file(&repo_path, "orphan.txt", "orphan\n", "orphan commit");

    checkpoint_batch_complete(&app, &session_id, "claude", &orphan_sha);
    let sends_before = wal_row_count(&app, &session_id, "collab_send");
    let err = call_tool_expect_error(
        &app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "implementation_done",
            "content": json!({ "head_sha": orphan_sha }).to_string()
        }),
    );
    assert!(
        err.contains("branch_drift:"),
        "expected branch_drift for a non-descendant head in the batch flow, got: {err}"
    );
    // Assert on stored state, not just the error: the error proves the call
    // was answered, only the row proves nothing was written.
    assert_eq!(
        wal_row_count(&app, &session_id, "collab_send"),
        sends_before,
        "a refused implementation_done must write no audit row: {err}"
    );
    let status = call_tool(&app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(
        status["phase"], "CodeImplementPending",
        "a refused implementation_done must not advance the phase: {err}"
    );
    assert_eq!(status["current_owner"], "claude");
    assert_ne!(
        status["last_head_sha"],
        json!(orphan_sha),
        "a refused implementation_done must not record the reported head: {err}"
    );
}
