//! With `IRONMEM_RERANK` unset, the rerank step must be a no-op.
//! We assert this by injecting a `PanicScorer` — if the pipeline ever calls
//! it, the test panics.

use std::sync::Arc;

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use ironrace_rerank::RerankerScorer;
use serde_json::{json, Value};

struct PanicScorer;
impl RerankerScorer for PanicScorer {
    fn score_pairs(&self, _q: &str, _p: &[&str]) -> anyhow::Result<Vec<f32>> {
        panic!("PanicScorer must NOT be called when IRONMEM_RERANK is unset");
    }
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
fn rerank_disabled_does_not_invoke_scorer() {
    std::env::remove_var("IRONMEM_RERANK");

    let app = App::with_reranker(Arc::new(PanicScorer)).expect("build app");

    // Ingest enough drawers to populate a meaningful candidate set.
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

    // Search must succeed AND the PanicScorer must NOT be invoked.
    let search = call(
        &app,
        "search",
        json!({ "query": "Rust memory safety", "limit": 10 }),
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "search should return results even with rerank disabled"
    );
}
