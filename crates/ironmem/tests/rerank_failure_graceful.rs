//! When the scorer returns Err, search must succeed and return the un-reranked
//! candidates (graceful degradation — never fail the whole query).

use std::sync::Arc;

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use ironrace_rerank::RerankerScorer;
use serde_json::{json, Value};

struct ErrScorer {
    called: std::sync::atomic::AtomicBool,
}
impl RerankerScorer for ErrScorer {
    fn score_pairs(&self, _q: &str, _p: &[&str]) -> anyhow::Result<Vec<f32>> {
        self.called.store(true, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("simulated scorer failure")
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
fn rerank_scorer_error_does_not_fail_search() {
    std::env::set_var("IRONMEM_RERANK", "llm_haiku");

    let scorer = Arc::new(ErrScorer {
        called: std::sync::atomic::AtomicBool::new(false),
    });
    let app = App::with_reranker(scorer.clone()).expect("build app");

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

    // Search MUST succeed even though the scorer errors out.
    let search = call(
        &app,
        "search",
        json!({ "query": "Rust memory safety", "limit": 10 }),
    );
    let results = search["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "search should still return results when scorer errors"
    );

    // Scorer was called (so we know the rerank stage actually ran and the
    // graceful-degradation branch is what kept the search alive).
    assert!(
        scorer.called.load(std::sync::atomic::Ordering::SeqCst),
        "ErrScorer must be invoked when IRONMEM_RERANK=llm_haiku"
    );
}
