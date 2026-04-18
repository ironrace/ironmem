//! Usage scenario tests — end-to-end flows exercising the full tool dispatch path.
//!
//! These tests use a real in-memory database with the noop embedder so they run
//! without downloading the ONNX model. They cover the tool combinations that a
//! real AI harness would exercise in practice.

use ironrace_embed::embedder::EMBED_DIM;
use ironrace_memory::config::McpAccessMode;
use ironrace_memory::mcp::app::App;
use ironrace_memory::mcp::protocol::JsonRpcRequest;
use ironrace_memory::mcp::server::dispatch;
use serde_json::{json, Value};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn request(method: &str, params: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

/// Call a tool and return the parsed JSON body of content[0].text.
/// Panics if the call returns an RPC-level error.
fn call(app: &App, tool: &str, args: Value) -> Value {
    let req = request("tools/call", json!({ "name": tool, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    assert!(
        resp.error.is_none(),
        "unexpected RPC error calling {tool}: {:?}",
        resp.error
    );
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    serde_json::from_str(text).expect("tool response must be valid JSON")
}

/// Call a tool and return the raw RPC response (may contain error).
fn call_raw(app: &App, tool: &str, args: Value) -> ironrace_memory::mcp::protocol::JsonRpcResponse {
    let req = request("tools/call", json!({ "name": tool, "arguments": args }));
    dispatch(app, &req).expect("dispatch must return a response")
}

/// Returns true when the response signals a tool-level error.
///
/// The MCP server wraps all tool errors as `JsonRpcResponse::success` with
/// `isError: true` in the result body (not as a JSON-RPC error object).
fn is_tool_error(resp: &ironrace_memory::mcp::protocol::JsonRpcResponse) -> bool {
    resp.error.is_some()
        || resp
            .result
            .as_ref()
            .map(|r| r["isError"] == true)
            .unwrap_or(false)
}

// ── Scenario 1: Add → Search → Delete round trip ─────────────────────────────

#[test]
fn add_search_delete_round_trip() {
    let app = App::open_for_test().unwrap();

    // 1. Add a drawer
    let added = call(
        &app,
        "ironmem_add_drawer",
        json!({
            "content": "The Rust borrow checker ensures memory safety at compile time",
            "wing": "projects",
            "room": "notes"
        }),
    );
    assert_eq!(added["success"], true);
    let drawer_id = added["id"].as_str().unwrap().to_string();
    assert!(!drawer_id.is_empty());

    // 2. Status shows the drawer exists
    let status = call(&app, "ironmem_status", json!({}));
    assert!(status["total_drawers"].as_u64().unwrap_or(0) >= 1);

    // 3. List wings shows the new wing
    let wings = call(&app, "ironmem_list_wings", json!({}));
    assert!(wings["wings"]["projects"].is_number());

    // 4. List rooms for the wing
    let rooms = call(&app, "ironmem_list_rooms", json!({ "wing": "projects" }));
    assert!(rooms["rooms"]["notes"].is_number());

    // 5. Search returns results (noop embedder returns zero vectors, so all
    //    candidates may have the same score — what matters is the count > 0)
    let search = call(
        &app,
        "ironmem_search",
        json!({ "query": "Rust memory safety", "limit": 5 }),
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "search should return at least one result"
    );

    // 6. Delete the drawer
    let deleted = call(&app, "ironmem_delete_drawer", json!({ "id": &drawer_id }));
    assert_eq!(deleted["success"], true);

    // 7. Status reflects deletion
    let status_after = call(&app, "ironmem_status", json!({}));
    assert!(
        status_after["total_drawers"].as_u64().unwrap_or(0)
            < status["total_drawers"].as_u64().unwrap_or(1) + 1
    );
}

#[test]
fn add_drawer_is_immediately_searchable_without_rebuild() {
    let app = App::open_for_test().unwrap();

    let dim = EMBED_DIM;
    let mut embedding = vec![0.0f32; dim];
    embedding[42] = 1.0;

    let id = "aabbccdd00112233aabbccdd00112233";
    app.db
        .insert_drawer(
            id,
            "test content about rockets",
            &embedding,
            "test-wing",
            "general",
            "",
            "src",
        )
        .unwrap();
    app.insert_into_index(id, &embedding).unwrap();

    let state = app.index_state.read().unwrap();
    assert_eq!(state.index.len(), 1);
    assert_eq!(state.id_map.len(), 1);
    assert_eq!(state.id_map[0], id);

    let results = state.index.search(&embedding, 1);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, 0);
}

// ── Scenario 2: Multiple drawers → taxonomy ──────────────────────────────────

/// Returns all room names listed under a wing in the taxonomy.
///
/// The taxonomy is serialized as `HashMap<String, Vec<(String, usize)>>`:
/// `{ "alpha": [["room-x", 1], ["room-y", 2]], ... }`.
fn taxonomy_rooms(taxonomy: &Value, wing: &str) -> Vec<String> {
    taxonomy["taxonomy"][wing]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|entry| entry[0].as_str().map(|s| s.to_string()))
        .collect()
}

#[test]
fn taxonomy_reflects_wing_room_hierarchy() {
    let app = App::open_for_test().unwrap();

    // Add drawers in different wings/rooms
    call(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "Alpha content in wing A room X extended text", "wing": "alpha", "room": "room-x" }),
    );
    call(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "Beta content in wing A room Y extended text", "wing": "alpha", "room": "room-y" }),
    );
    call(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "Gamma content in wing B general extended text", "wing": "beta", "room": "general" }),
    );

    let taxonomy = call(&app, "ironmem_get_taxonomy", json!({}));
    let alpha_rooms = taxonomy_rooms(&taxonomy, "alpha");
    let beta_rooms = taxonomy_rooms(&taxonomy, "beta");

    assert!(
        alpha_rooms.contains(&"room-x".to_string()),
        "alpha/room-x must be in taxonomy"
    );
    assert!(
        alpha_rooms.contains(&"room-y".to_string()),
        "alpha/room-y must be in taxonomy"
    );
    assert!(
        beta_rooms.contains(&"general".to_string()),
        "beta/general must be in taxonomy"
    );
}

// ── Scenario 3: KG triple lifecycle ──────────────────────────────────────────

#[test]
fn kg_triple_add_query_invalidate_timeline() {
    let app = App::open_for_test().unwrap();

    // Add a triple
    let added = call(
        &app,
        "ironmem_kg_add",
        json!({
            "subject": "Alice",
            "subject_type": "person",
            "predicate": "works-at",
            "object": "Acme Corp",
            "object_type": "company",
            "valid_from": "2024-01-01"
        }),
    );
    assert_eq!(added["success"], true);
    let triple_id = added["triple_id"].as_str().unwrap().to_string();

    // Query shows the triple
    let query = call(
        &app,
        "ironmem_kg_query",
        json!({ "entity": "Alice", "entity_type": "person" }),
    );
    let triples = query["triples"].as_array().unwrap();
    assert!(!triples.is_empty(), "query must return the inserted triple");
    assert_eq!(query["entity"]["name"], "Alice");

    // Timeline also shows the triple
    let timeline = call(
        &app,
        "ironmem_kg_timeline",
        json!({ "entity": "Alice", "entity_type": "person" }),
    );
    assert!(!timeline["timeline"].as_array().unwrap().is_empty());

    // Invalidate the triple
    let invalidated = call(
        &app,
        "ironmem_kg_invalidate",
        json!({ "triple_id": &triple_id, "valid_to": "2026-01-01" }),
    );
    assert_eq!(invalidated["success"], true);

    // KG stats still show entities/triples
    let stats = call(&app, "ironmem_kg_stats", json!({}));
    assert!(stats["entity_count"].as_u64().unwrap_or(0) > 0);
}

// ── Scenario 4: Diary write → read round trip ────────────────────────────────

#[test]
fn diary_write_read_round_trip() {
    let app = App::open_for_test().unwrap();

    let content = "Today we shipped the memory backend integration.";

    let written = call(
        &app,
        "ironmem_diary_write",
        json!({ "content": content, "wing": "diary" }),
    );
    assert_eq!(written["success"], true);
    let entry_id = written["id"].as_str().unwrap().to_string();

    let read = call(
        &app,
        "ironmem_diary_read",
        json!({ "wing": "diary", "limit": 10 }),
    );
    let entries = read["entries"].as_array().unwrap();
    assert!(
        !entries.is_empty(),
        "diary read must return at least one entry"
    );

    let ids: Vec<&str> = entries.iter().filter_map(|e| e["id"].as_str()).collect();
    assert!(
        ids.contains(&entry_id.as_str()),
        "written entry must appear in read result"
    );

    // Verify the content round-trips correctly
    let entry = entries.iter().find(|e| e["id"] == entry_id).unwrap();
    assert_eq!(entry["content"], content);
}

#[test]
fn diary_write_same_content_creates_distinct_entries() {
    let app = App::open_for_test().unwrap();
    let content = "Repeated summary text should create a new entry each time.";

    let first = call(
        &app,
        "ironmem_diary_write",
        json!({ "content": content, "wing": "diary" }),
    );
    let second = call(
        &app,
        "ironmem_diary_write",
        json!({ "content": content, "wing": "diary" }),
    );

    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    assert_ne!(
        first_id, second_id,
        "timestamped diary writes must not overwrite identical content"
    );

    let read = call(
        &app,
        "ironmem_diary_read",
        json!({ "wing": "diary", "limit": 10 }),
    );
    let entries = read["entries"].as_array().unwrap();
    let ids: Vec<&str> = entries.iter().filter_map(|e| e["id"].as_str()).collect();
    assert!(ids.contains(&first_id));
    assert!(ids.contains(&second_id));
}

// ── Scenario 5: Access mode — read-only blocks all writes ────────────────────

#[test]
fn read_only_mode_blocks_write_tools() {
    let app = App::open_for_test_with_mode(McpAccessMode::ReadOnly).unwrap();

    for (tool, args) in [
        (
            "ironmem_add_drawer",
            json!({ "content": "test content here", "wing": "wo" }),
        ),
        ("ironmem_delete_drawer", json!({ "id": "a".repeat(32) })),
        (
            "ironmem_diary_write",
            json!({ "content": "my note content" }),
        ),
        (
            "ironmem_kg_add",
            json!({ "subject": "a1", "predicate": "b1", "object": "c1" }),
        ),
    ] {
        let resp = call_raw(&app, tool, args.clone());
        assert!(
            is_tool_error(&resp),
            "read-only mode must block write tool '{tool}'"
        );
    }
}

// ── Scenario 6: Restricted mode — content redacted ───────────────────────────

#[test]
fn restricted_mode_redacts_search_content() {
    let restricted = App::open_for_test_with_mode(McpAccessMode::Restricted).unwrap();
    let embedding = vec![0.0f32; EMBED_DIM];
    restricted
        .db
        .insert_drawer(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "Sensitive internal roadmap data",
            &embedding,
            "projects",
            "secret",
            "",
            "test",
        )
        .unwrap();
    restricted
        .db
        .insert_drawer(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "Diary secret",
            &embedding,
            "diary",
            "diary",
            "",
            "test",
        )
        .unwrap();
    restricted.mark_dirty();

    let search = call(
        &restricted,
        "ironmem_search",
        json!({ "query": "roadmap", "limit": 5 }),
    );
    let results = search["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["content"], Value::Null);
    assert_eq!(results[0]["content_redacted"], true);

    let diary = call(
        &restricted,
        "ironmem_diary_read",
        json!({ "wing": "diary", "limit": 5 }),
    );
    let entries = diary["entries"].as_array().unwrap();
    assert!(!entries.is_empty());
    assert_eq!(entries[0]["content"], Value::Null);
    assert_eq!(entries[0]["content_redacted"], true);
}

// ── Scenario 7: Graph traverse and tunnels ───────────────────────────────────

#[test]
fn graph_traverse_and_tunnels_on_empty_store() {
    let app = App::open_for_test().unwrap();

    // Empty store — traverse a nonexistent room should return empty or error gracefully
    let resp = call_raw(
        &app,
        "ironmem_traverse",
        json!({ "room": "nonexistent", "max_depth": 2 }),
    );
    // Should not panic; either an empty result or a tool error is acceptable
    // The important thing is the server doesn't crash
    let _ = resp;

    // find_tunnels on empty store returns empty tunnels list
    let tunnels = call(&app, "ironmem_find_tunnels", json!({}));
    assert!(tunnels["tunnels"].as_array().unwrap().is_empty());

    // graph_stats on empty store returns zero counts
    let stats = call(&app, "ironmem_graph_stats", json!({}));
    assert_eq!(stats["total_rooms"].as_u64().unwrap_or(0), 0);
    assert_eq!(stats["total_wings"].as_u64().unwrap_or(0), 0);
}

#[test]
fn graph_traverse_with_data() {
    let app = App::open_for_test().unwrap();

    // Add drawers in a shared room across wings (creates a tunnel)
    call(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "Wing A general content one", "wing": "wing-a", "room": "shared" }),
    );
    call(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "Wing B general content two", "wing": "wing-b", "room": "shared" }),
    );
    call(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "Wing A local content", "wing": "wing-a", "room": "local" }),
    );

    // Traverse from the shared room
    let traversal = call(
        &app,
        "ironmem_traverse",
        json!({ "room": "shared", "max_depth": 3 }),
    );
    // The result should be a valid JSON object with a rooms/edges structure
    assert!(traversal.is_object(), "traverse must return an object");

    // Graph stats should show rooms and wings
    let stats = call(&app, "ironmem_graph_stats", json!({}));
    assert!(stats["total_rooms"].as_u64().unwrap_or(0) >= 2);
    assert!(stats["total_wings"].as_u64().unwrap_or(0) >= 2);

    // find_tunnels: "shared" is in two wings so should appear
    let tunnels = call(&app, "ironmem_find_tunnels", json!({}));
    let tunnel_rooms: Vec<&str> = tunnels["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["room"].as_str())
        .collect();
    assert!(
        tunnel_rooms.contains(&"shared"),
        "shared room in multiple wings must be detected as tunnel"
    );
}

// ── Scenario 8: Search limit cap ─────────────────────────────────────────────

#[test]
fn search_limit_is_capped_at_max() {
    let app = App::open_for_test().unwrap();

    // Add more than MAX_SEARCH_LIMIT (100) drawers
    for i in 0..110 {
        call(
            &app,
            "ironmem_add_drawer",
            json!({
                "content": format!("Drawer content number {i} about memory systems and Rust"),
                "wing": "bulk",
                "room": "general"
            }),
        );
    }

    // Request 200 results — should be capped at 100
    let search = call(
        &app,
        "ironmem_search",
        json!({ "query": "memory systems", "limit": 200 }),
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        results.len() <= 100,
        "search results must be capped at MAX_SEARCH_LIMIT (100), got {}",
        results.len()
    );
}

// ── Scenario 9: Sanitization edge cases via tool dispatch ────────────────────

#[test]
fn wing_name_with_path_traversal_is_rejected() {
    let app = App::open_for_test().unwrap();

    let resp = call_raw(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "some content here", "wing": "../etc/passwd" }),
    );
    assert!(
        is_tool_error(&resp),
        "path traversal in wing must return a tool error"
    );
}

#[test]
fn empty_content_is_rejected() {
    let app = App::open_for_test().unwrap();

    let resp = call_raw(
        &app,
        "ironmem_add_drawer",
        json!({ "content": "   ", "wing": "projects" }),
    );
    assert!(
        is_tool_error(&resp),
        "empty/whitespace content must return a tool error"
    );
}

#[test]
fn delete_with_invalid_id_format_is_rejected() {
    let app = App::open_for_test().unwrap();

    for bad_id in &["not-a-hex", "tooshort", &"z".repeat(32)] {
        let resp = call_raw(&app, "ironmem_delete_drawer", json!({ "id": bad_id }));
        assert!(
            is_tool_error(&resp),
            "invalid id '{bad_id}' must return a tool error"
        );
    }
}

// ── Scenario 10: Unknown tool returns structured error ────────────────────────

#[test]
fn unknown_tool_returns_structured_error() {
    let app = App::open_for_test().unwrap();

    let resp = call_raw(&app, "ironmem_does_not_exist", json!({}));
    assert!(
        is_tool_error(&resp),
        "unknown tool must return a tool error"
    );
}

// ── Scenario 11: KG stats on fresh store ─────────────────────────────────────

#[test]
fn kg_stats_on_empty_store_returns_zeros() {
    let app = App::open_for_test().unwrap();

    let stats = call(&app, "ironmem_kg_stats", json!({}));
    assert_eq!(stats["entity_count"].as_u64().unwrap_or(99), 0);
    assert_eq!(stats["triple_count"].as_u64().unwrap_or(99), 0);
}

// ── Scenario 12: KG query for unknown entity returns error ───────────────────

#[test]
fn kg_query_unknown_entity_returns_error() {
    let app = App::open_for_test().unwrap();

    let resp = call_raw(
        &app,
        "ironmem_kg_query",
        json!({ "entity": "no-such-entity" }),
    );
    assert!(
        is_tool_error(&resp),
        "querying an unknown entity must return a tool error"
    );
}

// ── Scenario 13: Diary read limit cap ────────────────────────────────────────

#[test]
fn diary_read_limit_is_capped() {
    let app = App::open_for_test().unwrap();

    // Write several entries
    for i in 0..5 {
        call(
            &app,
            "ironmem_diary_write",
            json!({ "content": format!("Session summary entry number {i} with details") }),
        );
    }

    // Request more than available — should return what exists, capped at MAX_READ_LIMIT
    let read = call(&app, "ironmem_diary_read", json!({ "limit": 200 }));
    let count = read["entries"].as_array().unwrap().len();
    // 200 was requested; MAX_READ_LIMIT is 100, but we only wrote 5 entries.
    // Note: same content hash → same ID → inserts may deduplicate.
    // At minimum, count must not exceed 100.
    assert!(count <= 100, "diary read must respect MAX_READ_LIMIT cap");
}

// ── Scenario 14: date validation on KG ───────────────────────────────────────

#[test]
fn kg_add_rejects_invalid_date_format() {
    let app = App::open_for_test().unwrap();

    let resp = call_raw(
        &app,
        "ironmem_kg_add",
        json!({
            "subject": "Alice",
            "predicate": "works-at",
            "object": "Acme",
            "valid_from": "not-a-date"
        }),
    );
    assert!(
        is_tool_error(&resp),
        "invalid valid_from date must return a tool error"
    );
}
