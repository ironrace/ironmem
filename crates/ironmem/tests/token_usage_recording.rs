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

use std::sync::{Arc, Mutex};

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

static PREF_ENV_LOCK: Mutex<()> = Mutex::new(());

/// EXIT CRITERION — `add_drawer` with a mock LLM pref-extractor records exactly
/// one `source="pref_extract"` token_usage row carrying the mock's usage.
#[test]
fn add_drawer_with_llm_pref_extractor_records_token_usage() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

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
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::remove_var("IRONMEM_PREF_ENRICH");

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
    let rerank_rows: Vec<_> = rows.iter().filter(|r| r.source == "llm_rerank").collect();
    assert_eq!(rerank_rows.len(), 1, "exactly one llm_rerank row expected");
    let row = rerank_rows[0];
    assert_eq!(row.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(row.input_tokens, 800);
    assert_eq!(row.output_tokens, 1);
    assert!(!row.estimated);
    assert_eq!(row.chars, 3001);
}

/// RERANK NEGATIVE — an LLM call that succeeds but returns an unparsable answer
/// still records usage before the rerank stage gracefully falls back.
#[test]
fn search_records_llm_rerank_usage_when_response_is_unparseable() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::remove_var("IRONMEM_PREF_ENRICH");

    let mock = MockLlmClient::ok_response(LlmResponse {
        text: "no numeric answer".to_string(),
        usage: Usage {
            input_tokens: 700,
            output_tokens: 4,
            ..Default::default()
        },
        cost_usd: None,
        model: "claude-haiku-4-5".to_string(),
        estimated: false,
        prompt_chars: 2048,
    });
    let scorer: Arc<dyn ironrace_rerank::RerankerScorer> = Arc::new(LlmReranker::new(mock));
    let app = App::with_reranker_forced(scorer).expect("build app with forced reranker");

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
        "search should gracefully return fallback results"
    );

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    let rerank_rows: Vec<_> = rows.iter().filter(|r| r.source == "llm_rerank").collect();
    assert_eq!(
        rerank_rows.len(),
        1,
        "usage row should survive parse failure"
    );
    let row = rerank_rows[0];
    assert_eq!(row.model.as_deref(), Some("claude-haiku-4-5"));
    assert_eq!(row.input_tokens, 700);
    assert_eq!(row.output_tokens, 4);
    assert!(!row.estimated);
    assert_eq!(row.chars, 2048 + "no numeric answer".chars().count() as i64);
}

/// NEGATIVE — when the LLM pref call errors, `add_drawer` still succeeds and NO
/// `pref_extract` row is recorded (failure path returns `(phrases, None)`).
#[test]
fn failed_llm_pref_call_records_no_row() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

    let extractor = Arc::new(LlmPreferenceExtractor::new(Arc::new(MockLlmClient::err(
        "boom",
    ))));
    let app = App::with_pref_extractor(extractor).expect("build app with pref extractor");

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

// ---------------------------------------------------------------------------
// Task 3: collab session + task-tag stamping tests
// ---------------------------------------------------------------------------

/// Helper: seed a collab session via the queue layer (same pattern used in
/// `metrics/mod.rs` unit tests) and return its id.
fn seed_collab_session(app: &App, sid: &str) {
    app.db
        .with_transaction(|tx| {
            ironmem::collab::queue::create_session(
                tx,
                sid,
                "/tmp/repo",
                "main",
                None,
                ironmem::collab::CollabRoles {
                    pilot: ironmem::collab::Agent::Claude,
                    implementer: ironmem::collab::Agent::Claude,
                },
            )
        })
        .expect("seed_collab_session must succeed");
}

fn app_with_forced_reranker(input_tokens: u32) -> App {
    let mock = MockLlmClient::ok_response(LlmResponse {
        text: "1".to_string(),
        usage: Usage {
            input_tokens,
            output_tokens: 1,
            ..Default::default()
        },
        cost_usd: None,
        model: "claude-haiku-4-5".to_string(),
        estimated: false,
        prompt_chars: 3000,
    });
    let scorer: Arc<dyn ironrace_rerank::RerankerScorer> = Arc::new(LlmReranker::new(mock));
    App::with_reranker_forced(scorer).expect("build app with forced reranker")
}

fn seed_rerank_search(app: &App) {
    for i in 0..15 {
        let added = call(
            app,
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
        app,
        "search",
        json!({ "query": "Rust memory safety", "limit": 5 }),
    );
    assert!(
        !search["results"].as_array().unwrap().is_empty(),
        "search should return results"
    );
}

/// pref_extract rows produced during an active collab session carry
/// collab_session_id + phase bucket (new session = PlanParallelDrafts → "planning").
#[test]
fn pref_extract_rows_are_stamped_during_active_collab_session() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

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
    let app = App::with_pref_extractor(extractor).expect("build app");

    let sid = "test-collab-pref-stamped";
    seed_collab_session(&app, sid);
    app.set_active_collab_session(sid);

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
    assert_eq!(pref_rows.len(), 1, "exactly one pref_extract row");

    let row = pref_rows[0];
    assert_eq!(
        row.collab_session_id.as_deref(),
        Some(sid),
        "row must carry the active collab session id"
    );
    assert_eq!(
        row.collab_phase.as_deref(),
        Some("planning"),
        "fresh session (PlanParallelDrafts) maps to 'planning'"
    );

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}

/// pref_extract rows produced WITHOUT an active collab session or task tag
/// have all three context columns set to None.
#[test]
fn pref_extract_rows_are_unstamped_without_collab_or_tag() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

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
    let app = App::with_pref_extractor(extractor).expect("build app");
    // Deliberately: no set_active_collab_session, no set_explicit_task_tag

    let added = call(
        &app,
        "add_drawer",
        json!({ "wing": "prefs", "room": "general", "content": CONVERSATION }),
    );
    assert_eq!(added["success"], true);

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    let pref_rows: Vec<_> = rows.iter().filter(|r| r.source == "pref_extract").collect();
    assert_eq!(pref_rows.len(), 1, "exactly one pref_extract row");

    let row = pref_rows[0];
    assert!(
        row.collab_session_id.is_none(),
        "no collab session → collab_session_id must be None"
    );
    assert!(
        row.collab_phase.is_none(),
        "no collab session or tag → collab_phase must be None"
    );
    assert!(
        row.task_tag.is_none(),
        "no task tag set → task_tag must be None"
    );

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}

/// An explicit task tag (no collab session) stamps task_tag and "impl" bucket.
#[test]
fn explicit_task_tag_stamps_rows_with_impl_phase() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::set_var("IRONMEM_PREF_ENRICH", "1");

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
    let app = App::with_pref_extractor(extractor).expect("build app");
    app.set_explicit_task_tag("issue-99");
    // No collab session — task tag path only.

    let added = call(
        &app,
        "add_drawer",
        json!({ "wing": "prefs", "room": "general", "content": CONVERSATION }),
    );
    assert_eq!(added["success"], true);

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    let pref_rows: Vec<_> = rows.iter().filter(|r| r.source == "pref_extract").collect();
    assert_eq!(pref_rows.len(), 1, "exactly one pref_extract row");

    let row = pref_rows[0];
    assert_eq!(
        row.task_tag.as_deref(),
        Some("issue-99"),
        "task_tag must be stamped"
    );
    assert_eq!(
        row.collab_phase.as_deref(),
        Some("impl"),
        "task-tag-only path defaults phase to 'impl' per §3.3"
    );
    assert!(
        row.collab_session_id.is_none(),
        "no collab session → collab_session_id must be None"
    );

    std::env::remove_var("IRONMEM_PREF_ENRICH");
}

/// llm_rerank rows produced during an active collab session carry
/// collab_session_id + phase bucket.
#[test]
fn llm_rerank_rows_are_stamped_during_active_collab_session() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::remove_var("IRONMEM_PREF_ENRICH");

    let app = app_with_forced_reranker(801);
    let sid = "test-collab-rerank-stamped";
    seed_collab_session(&app, sid);
    app.set_active_collab_session(sid);

    seed_rerank_search(&app);

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    let rerank_rows: Vec<_> = rows.iter().filter(|r| r.source == "llm_rerank").collect();
    assert_eq!(rerank_rows.len(), 1, "exactly one llm_rerank row");

    let row = rerank_rows[0];
    assert_eq!(
        row.collab_session_id.as_deref(),
        Some(sid),
        "row must carry the active collab session id"
    );
    assert_eq!(
        row.collab_phase.as_deref(),
        Some("planning"),
        "fresh session (PlanParallelDrafts) maps to 'planning'"
    );
    assert!(row.task_tag.is_none());
}

/// llm_rerank rows produced with an explicit task tag default to the impl bucket.
#[test]
fn llm_rerank_rows_are_stamped_with_explicit_task_tag() {
    let _guard = PREF_ENV_LOCK.lock().unwrap();
    std::env::remove_var("IRONMEM_PREF_ENRICH");

    let app = app_with_forced_reranker(802);
    app.set_explicit_task_tag("issue-99");

    seed_rerank_search(&app);

    let rows = app
        .db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query_token_usage");
    let rerank_rows: Vec<_> = rows.iter().filter(|r| r.source == "llm_rerank").collect();
    assert_eq!(rerank_rows.len(), 1, "exactly one llm_rerank row");

    let row = rerank_rows[0];
    assert_eq!(
        row.task_tag.as_deref(),
        Some("issue-99"),
        "task_tag must be stamped"
    );
    assert_eq!(
        row.collab_phase.as_deref(),
        Some("impl"),
        "task-tag-only path defaults phase to 'impl' per §3.3"
    );
    assert!(row.collab_session_id.is_none());
}
