//! Integration tests for the preference-enrichment ingest pass.

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::{json, Value};
use std::sync::Mutex;

// Serializes the IRONMEM_PREF_ENRICH-touching tests because the tunable was
// formerly process-cached. Even after switching to per-call env reads, we
// keep the mutex to prevent racy interleaving when tests set/unset the var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn request(method: &str, params: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

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

#[test]
fn deleting_parent_cascades_to_synthetic_sibling() {
    // Insert one parent + one synthetic sibling pointing at it directly via
    // the DB layer (so this test doesn't depend on Task 6's enrichment wiring).
    let app = App::open_for_test().expect("build test app");
    let parent_id = "a".repeat(32);
    let synth_id = "b".repeat(32);
    let zero_vec: Vec<f32> = vec![0.0; 384];

    app.db
        .insert_drawer(&parent_id, "parent", &zero_vec, "w", "r", "", "test")
        .unwrap();
    app.db
        .insert_drawer(
            &synth_id,
            "User has mentioned: thing",
            &zero_vec,
            "w",
            "r",
            &format!("pref:{parent_id}"),
            "test",
        )
        .unwrap();

    let deleted = call(&app, "delete_drawer", json!({ "id": parent_id }));
    assert_eq!(deleted["success"], true);

    // Synthetic sibling must be gone too.
    let got = app.db.get_drawer(&synth_id).unwrap();
    assert!(got.is_none(), "synthetic sibling should cascade-delete");
}

const CONVERSATIONAL_BODY: &str = "I've been having trouble with the battery life on my phone lately. \
I prefer carrying a small power bank when I travel. Lately, I've been thinking about switching to a \
phone with a removable battery. I usually plug in overnight.";

fn count_drawers(app: &App) -> usize {
    app.db.count_drawers(None).unwrap()
}

#[test]
fn enrich_off_inserts_only_one_row() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::remove_var("IRONMEM_PREF_ENRICH");
    let app = App::open_for_test().expect("build app");
    let added = call(
        &app,
        "add_drawer",
        json!({
            "content": CONVERSATIONAL_BODY,
            "wing": "diary",
            "room": "general"
        }),
    );
    assert_eq!(added["success"], true);
    assert_eq!(count_drawers(&app), 1);
}

#[test]
fn enrich_on_inserts_parent_plus_synthetic() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");
    let app = App::open_for_test().expect("build app");
    let added = call(
        &app,
        "add_drawer",
        json!({
            "content": CONVERSATIONAL_BODY,
            "wing": "diary",
            "room": "general"
        }),
    );
    assert_eq!(added["success"], true);

    // Two rows: the parent, and a sibling whose source_file is "pref:<parent>".
    assert_eq!(count_drawers(&app), 2);
    let parent_id = added["id"].as_str().unwrap();
    let sentinel = format!("pref:{parent_id}");
    let siblings = app
        .db
        .get_drawers(None, None, 100)
        .unwrap()
        .into_iter()
        .filter(|d| d.source_file == sentinel)
        .collect::<Vec<_>>();
    assert_eq!(siblings.len(), 1, "exactly one synthetic sibling");
    // Synth content is bare phrases joined by ". " (no meta prefix). The
    // CONVERSATIONAL_BODY mentions trouble with battery life, so the
    // extractor should produce at least one phrase referencing it.
    assert!(
        siblings[0].content.contains("battery"),
        "synth body should carry vocabulary from the source: {:?}",
        siblings[0].content
    );

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}

#[test]
fn enrich_on_skips_non_conversational_input() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");
    let app = App::open_for_test().expect("build app");
    let rust_source = "fn main() { let x = 42; println!(\"{}\", x); }";
    let added = call(
        &app,
        "add_drawer",
        json!({ "content": rust_source, "wing": "code", "room": "rust" }),
    );
    assert_eq!(added["success"], true);
    assert_eq!(count_drawers(&app), 1, "non-conversational → no sibling");

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}
