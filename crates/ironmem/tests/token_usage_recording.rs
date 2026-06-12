//! Exit-criteria integration tests for token_usage recording (issue #81 / PR 03).
//!
//! These exercise the deterministic test seams added in Task 6:
//!   * `App::with_pref_extractor` installs a concrete `LlmPreferenceExtractor`
//!     so `build_synthetic` records a `pref_extract` row without a live LLM.
//!   * `App::with_reranker_forced` sets `force_rerank`, so the rerank stage runs
//!     even with the OnceLock-cached `IRONMEM_RERANK` gate unset, recording an
//!     `llm_rerank` row.
//!
//! Each `tests/*.rs` is its own binary, so the OnceLock tunable gates start
//! fresh here. We still scope env vars within each test and never depend on
//! cross-test ordering.

use std::sync::Arc;

use ironmem::db::metrics::TokenUsageQuery;
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use ironmem::search::pref_extract_llm::LlmPreferenceExtractor;
use ironrace_rerank::{LlmReranker, LlmResponse, MockLlmClient, Usage};
use serde_json::{json, Value};

fn request(method: &str, params: Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

/// Dispatch a tools/call through the real MCP entry point and return the parsed
/// tool JSON payload (the production path `add_drawer`/`search` flow through).
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

/// Conversational content so `looks_conversational()` passes and the synthetic
/// preference drawer is attempted.
const CONVERSATION: &str =
    "User: I'm upgrading my Sony A7R IV.\nAssistant: Great, what lenses?\nUser: I want a flash and a tripod for photography.";

/// The claude -p envelope the mock returns; `extract_assistant_text` unwraps the
/// `result` field. Its char count feeds `chars` (prompt_chars + text chars).
const ENVELOPE: &str =
    r#"{"type":"result","result":"photography accessories, camera flash, tripod"}"#;

/// EXIT CRITERION — `add_drawer` with a mock LLM pref-extractor records exactly
/// one `source="pref_extract"` token_usage row carrying the mock's usage.
#[test]
fn add_drawer_with_llm_pref_extractor_records_token_usage() {
    let mock = MockLlmClient::ok_response(LlmResponse {
        text: ENVELOPE.to_string(),
        usage: Usage {
            input_tokens: 130,
            output_tokens: 9,
            ..Default::default()
        },
        cost_usd: Some(0.0009),
        model: "claude-haiku-4-5".to_string(),
        estimated: false,
        prompt_chars: 512,
    });
    let extractor = Arc::new(LlmPreferenceExtractor::new(Arc::new(mock)));
    let app = App::with_pref_extractor(extractor).expect("build app with pref extractor");

    // `pref_enrich_enabled()` is NOT OnceLock-cached — safe to flip per-test.
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

    let added = call(
        &app,
        "add_drawer",
        json!({ "wing": "prefs", "room": "general", "content": CONVERSATION }),
    );
    assert_eq!(added["success"], true, "add_drawer should succeed");

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    let pref_rows: Vec<_> = rows.iter().filter(|r| r.source == "pref_extract").collect();
    assert_eq!(pref_rows.len(), 1, "exactly one pref_extract row expected");

    let row = pref_rows[0];
    assert_eq!(row.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(row.input_tokens, 130);
    assert_eq!(row.output_tokens, 9);
    assert!(!row.estimated, "real usage block → not estimated");
    assert_eq!(
        row.chars,
        512 + ENVELOPE.chars().count() as i64,
        "chars = prompt_chars + assistant-text char count"
    );

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}

/// RERANK — a search through the real pipeline with an installed
/// `LlmReranker<MockLlmClient>` records exactly one `source="llm_rerank"` row.
/// `force_rerank` (set by `with_reranker_forced`) runs the stage without
/// IRONMEM_RERANK being set.
#[test]
fn search_with_llm_reranker_records_llm_rerank_usage() {
    // Mock reranker returns "1" (rank the single candidate first) with a real
    // usage block so the recorded row is non-estimated.
    let mock = MockLlmClient::ok_response(LlmResponse {
        text: "1".to_string(),
        usage: Usage {
            input_tokens: 800,
            output_tokens: 1,
            ..Default::default()
        },
        cost_usd: None,
        model: "claude-haiku-4-5".to_string(),
        estimated: false,
        prompt_chars: 3000,
    });
    let scorer: Arc<dyn ironrace_rerank::RerankerScorer> = Arc::new(LlmReranker::new(mock));
    let app = App::with_reranker_forced(scorer).expect("build app with forced reranker");

    // Seed enough drawers that the rerank window is non-empty, mirroring the
    // existing rerank integration tests.
    for i in 0..15 {
        let added = call(
            &app,
            "add_drawer",
            json!({
                "content": format!("Rust memory safety topic number {i} discussing borrow checker and ownership"),
                "wing": "projects",
                "room": "notes"
            }),
        );
        assert_eq!(added["success"], true);
    }

    let search = call(
        &app,
        "search",
        json!({ "query": "Rust memory safety", "limit": 5 }),
    );
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "search should return results"
    );

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    assert_eq!(
        rows.iter().filter(|r| r.source == "llm_rerank").count(),
        1,
        "exactly one llm_rerank row expected"
    );
}

/// NEGATIVE — when the LLM pref call errors, `add_drawer` still succeeds and NO
/// `pref_extract` row is recorded (failure path returns `(phrases, None)`).
#[test]
fn failed_llm_pref_call_records_no_row() {
    let extractor = Arc::new(LlmPreferenceExtractor::new(Arc::new(MockLlmClient::err(
        "boom",
    ))));
    let app = App::with_pref_extractor(extractor).expect("build app with pref extractor");

    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

    let added = call(
        &app,
        "add_drawer",
        json!({ "wing": "prefs", "room": "general", "content": CONVERSATION }),
    );
    assert_eq!(
        added["success"], true,
        "add_drawer still succeeds on LLM failure"
    );

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    assert!(
        rows.iter().all(|r| r.source != "pref_extract"),
        "no pref_extract row when the LLM call errors"
    );

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}
