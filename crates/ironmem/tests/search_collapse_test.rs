//! Pipeline-level tests for the synthetic-doc collapse step (step 7.5).

use ironmem::db::ScoredDrawer;
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use ironmem::search::collapse_synthetic_into_parents;
use serde_json::{json, Value};

fn fixture_drawer(id: &str, content: &str, source_file: &str, score: f32) -> ScoredDrawer {
    ScoredDrawer {
        drawer: ironmem::db::Drawer {
            id: id.to_string(),
            content: content.to_string(),
            wing: "w".to_string(),
            room: "r".to_string(),
            source_file: source_file.to_string(),
            added_by: "test".to_string(),
            filed_at: "2026-04-29".to_string(),
            date: "2026-04-29".to_string(),
        },
        score,
    }
}

#[test]
fn synth_above_parent_promotes_parent_score_and_drops_synth() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "p".repeat(32);
    let synth_id = "s".repeat(32);

    // Parent already loaded; synth ranks higher.
    let mut scored = vec![
        fixture_drawer(
            &synth_id,
            "User has mentioned: x",
            &format!("pref:{parent_id}"),
            0.9,
        ),
        fixture_drawer(&parent_id, "parent body", "", 0.4),
    ];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    // Synth dropped; parent remains with the elevated score.
    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].drawer.id, parent_id);
    assert!((scored[0].score - 0.9).abs() < 1e-6);
}

#[test]
fn parent_above_synth_keeps_parent_unchanged_and_drops_synth() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "p".repeat(32);
    let synth_id = "s".repeat(32);

    let mut scored = vec![
        fixture_drawer(&parent_id, "parent body", "", 0.7),
        fixture_drawer(
            &synth_id,
            "User has mentioned: x",
            &format!("pref:{parent_id}"),
            0.5,
        ),
    ];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].drawer.id, parent_id);
    assert!((scored[0].score - 0.7).abs() < 1e-6);
}

#[test]
fn orphan_synth_fetches_missing_parent_when_present_in_db() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "a".repeat(32);
    let synth_id = "b".repeat(32);
    let zero_vec: Vec<f32> = vec![0.0; 384];

    // Insert parent in DB but NOT in `scored` (parent didn't make HNSW top-N).
    app.db
        .insert_drawer(&parent_id, "parent body", &zero_vec, "w", "r", "", "test")
        .unwrap();
    app.db
        .insert_drawer(
            &synth_id,
            "User has mentioned: x",
            &zero_vec,
            "w",
            "r",
            &format!("pref:{parent_id}"),
            "test",
        )
        .unwrap();

    let mut scored = vec![fixture_drawer(
        &synth_id,
        "User has mentioned: x",
        &format!("pref:{parent_id}"),
        0.8,
    )];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    // Synth dropped; parent surfaced from DB with synth's score.
    assert_eq!(scored.len(), 1);
    assert_eq!(scored[0].drawer.id, parent_id);
    assert!((scored[0].score - 0.8).abs() < 1e-6);
}

#[test]
fn orphan_synth_with_deleted_parent_drops_quietly() {
    let app = App::open_for_test().expect("build app");
    let parent_id = "z".repeat(32);
    let synth_id = "y".repeat(32);

    // Parent is NOT in DB and NOT in `scored`.
    let mut scored = vec![fixture_drawer(
        &synth_id,
        "User has mentioned: x",
        &format!("pref:{parent_id}"),
        0.6,
    )];

    collapse_synthetic_into_parents(&app, &mut scored).unwrap();

    assert!(scored.is_empty(), "orphan synth without parent must drop");
}

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
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    serde_json::from_str(text).expect("tool response must be valid JSON")
}

#[test]
fn search_response_never_contains_synthetic_id() {
    // End-to-end: with enrichment ON, search for the conversational topic and
    // verify no result's id matches a synthetic sibling row in the DB. The
    // collapse step must translate any synth hit into its parent before the
    // results leave the pipeline.
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");
    let app = App::open_for_test().expect("build app");

    let _ = call(
        &app,
        "add_drawer",
        json!({
            "content": "I've been having trouble with the battery life on my phone. \
                        I prefer carrying a small power bank when I travel.",
            "wing": "diary",
            "room": "general"
        }),
    );

    // Snapshot synth IDs from the DB (rows with the `pref:<id>` sentinel).
    let synth_ids: std::collections::HashSet<String> = app
        .db
        .get_drawers(None, None, 100)
        .unwrap()
        .into_iter()
        .filter(|d| d.source_file.starts_with("pref:"))
        .map(|d| d.id)
        .collect();
    assert!(
        !synth_ids.is_empty(),
        "test setup expects at least one synth sibling"
    );

    let resp = call(&app, "search", json!({ "query": "battery", "limit": 10 }));
    let results = resp["results"].as_array().unwrap();
    for r in results {
        let id = r["id"].as_str().unwrap_or("");
        assert!(
            !synth_ids.contains(id),
            "synthetic doc leaked into search response: id={id}",
        );
    }
    std::env::remove_var("IRONMEM_PREF_ENRICH");
}
