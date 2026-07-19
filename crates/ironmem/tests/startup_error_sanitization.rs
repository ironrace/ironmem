//! Startup failures must not leak internal detail to MCP clients.
//!
//! When background memory init (`run_background_memory_init` → `App::new`)
//! fails, the readiness gate resolves `Failed(reason)`. That `reason` is not
//! an internal-only breadcrumb: `ReadinessGate::wait_for_write` embeds it in
//! `MemoryError::NotReady`, and `mcp::server::tool_error_response` forwards a
//! `NotReady` message to the client verbatim. So whatever string startup hands
//! to `resolve_failed` is echoed to every MCP client that attempts a write
//! during the failed warm-up window.
//!
//! This test drives that whole path for real — a `Config` pointed at an
//! unopenable database, the real background init thread, and the real
//! wire-level `dispatch` entry point — and asserts the client-visible text
//! carries no filesystem path and no underlying OS/driver error text. Full
//! detail must still be available server-side via `tracing`, which this test
//! deliberately does not constrain.

use std::sync::Arc;
use std::time::Duration;

use ironmem::bootstrap::run_background_memory_init;
use ironmem::config::{Config, EmbedMode, McpAccessMode};
use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::readiness::ReadinessGate;
use ironmem::mcp::server::dispatch;
use serde_json::{json, Value};

/// A distinctive path segment: if any part of the failing `db_path` reaches
/// the client, this substring reaches it too.
const SECRET_PATH_SEGMENT: &str = "ironmem-private-store-marker";

/// Builds a `Config` whose `db_path` lives under a *regular file*, so
/// `Config::ensure_dirs`'s `create_dir_all` fails and `App::new` errors out —
/// a realistic startup failure that needs no permission games or missing
/// system state.
fn config_that_fails_to_start(dir: &std::path::Path) -> Config {
    let blocking_file = dir.join("blocker");
    std::fs::write(&blocking_file, b"not a directory").expect("write blocker file");

    Config {
        db_path: blocking_file
            .join(SECRET_PATH_SEGMENT)
            .join("memory.sqlite3"),
        model_dir: dir.join("models"),
        model_dir_explicit: true,
        state_dir: dir.join("state"),
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    }
}

/// Drives `add_drawer` through the real wire-level `dispatch` entry point and
/// returns the client-visible error text.
fn client_visible_write_error(app: &App) -> String {
    let request: JsonRpcRequest = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "add_drawer",
            "arguments": { "content": "any content", "wing": "ironrace-memory" },
        },
    }))
    .expect("request fixture must deserialize");

    let response = dispatch(app, &request).expect("tools/call must return a response");
    let result = response.result.expect("tool result must be present");
    assert_eq!(
        result["isError"].as_bool(),
        Some(true),
        "a write during a failed warm-up must be an error: {result}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    let payload: Value = serde_json::from_str(text).expect("tool response text must be valid JSON");
    payload["error"]
        .as_str()
        .expect("error payload must carry a string message")
        .to_string()
}

#[test]
fn failed_startup_does_not_leak_internal_detail_to_mcp_clients() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = config_that_fails_to_start(dir.path());
    let failing_db_path = config.db_path.clone();

    let gate = Arc::new(ReadinessGate::new_pending());
    run_background_memory_init(config, Arc::clone(&gate));

    // Block until the background thread resolves the gate. It resolves
    // `Failed` here, so this returns `Err` — the same call every write-shaped
    // tool handler makes.
    let wait_result = gate.wait_for_write(Duration::from_secs(30));
    assert!(
        wait_result.is_err(),
        "an unopenable database must resolve the readiness gate as Failed"
    );

    let mut app = App::open_for_test().expect("test app");
    app.memory_ready = Arc::clone(&gate);
    let message = client_visible_write_error(&app);

    assert!(
        !message.contains(SECRET_PATH_SEGMENT),
        "startup failure leaked a filesystem path to the client: {message}"
    );
    assert!(
        !message.contains(&failing_db_path.to_string_lossy().to_string()),
        "startup failure leaked the database path to the client: {message}"
    );
    for internal_fragment in ["os error", "Not a directory", "No such file", "App::new"] {
        assert!(
            !message.contains(internal_fragment),
            "startup failure leaked underlying error text {internal_fragment:?} to the \
             client: {message}"
        );
    }
    assert!(
        !message.is_empty(),
        "the client still needs an actionable, non-empty message"
    );
}
