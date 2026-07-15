//! MCP server — JSON-RPC 2.0 over stdio.

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::app::{harness_from_client_info, session_id_from_params, App};
use super::protocol::{self, JsonRpcRequest, JsonRpcResponse};
use super::tools;
use crate::error::MemoryError;

/// Per-connection metrics attribution: harness + session id learned from
/// *this* connection's own `initialize` request. Local to one
/// `run_framing_loop` invocation (one per connection) so that when a single
/// `App` is shared across multiple concurrent/sequential connections (the
/// shared-daemon transport this crate is moving toward), one connection's
/// `initialize` can never clobber — or be blocked by — another connection's
/// learned attribution. This replaces the pre-existing `App::session_id` /
/// `App::harness` fields, which were process-global set-once cells: correct
/// for a single bare-stdio connection, but silently mis-attributing every
/// subsequent connection's requests to the first connection's session/harness
/// once shared.
#[derive(Default)]
struct ConnectionContext {
    session_id: Option<String>,
    harness: Option<String>,
}

impl ConnectionContext {
    /// Seed from the `IRONMEM_SESSION_ID` env override, matching the
    /// pre-existing `App::session_id` construction-time seed. An env value
    /// takes priority over anything `initialize` supplies (`learn` never
    /// overwrites an already-`Some` value), exactly mirroring prior behavior.
    fn new() -> Self {
        Self {
            session_id: std::env::var("IRONMEM_SESSION_ID").ok(),
            harness: None,
        }
    }

    /// Learn session id + harness from an `initialize` request's params.
    /// Set-once per connection: never overwrites an already-learned value.
    fn learn(&mut self, params: &serde_json::Value) {
        if self.session_id.is_none() {
            self.session_id = session_id_from_params(params);
        }
        if self.harness.is_none() {
            self.harness = harness_from_client_info(params);
        }
    }
}

fn mcp_harness(ctx: &ConnectionContext) -> String {
    if let Ok(value) = std::env::var("IRONMEM_HARNESS") {
        if let Some(id) = crate::harness::canonicalize_input(&value, crate::harness::REGISTRY) {
            return id.to_string();
        }
    }
    ctx.harness.clone().unwrap_or_else(|| "claude".to_string())
}

/// Collab tool calls carry a `session_id` argument; use it as the D1 fallback
/// key for `mcp_chars_served` when no harness session id was learned.
fn request_collab_session_id(request: &JsonRpcRequest) -> Option<String> {
    if request.method != "tools/call" {
        return None;
    }
    let tool_name = request.params.get("name").and_then(|v| v.as_str())?;
    if !tool_name.starts_with("collab_") {
        return None;
    }
    request
        .params
        .get("arguments")
        .and_then(|a| a.get("session_id"))
        .and_then(|v| v.as_str())
        .and_then(normalize_session_id)
}

fn normalize_session_id(value: &str) -> Option<String> {
    let sanitized = crate::sanitize::sanitize_session_id(value);
    if sanitized == "unknown" {
        None
    } else {
        Some(sanitized)
    }
}

fn account_response_metrics(
    app: &App,
    conn: &ConnectionContext,
    chars: usize,
    tool_name: Option<&str>,
    session_id: Option<&str>,
    exploration: Option<&crate::metrics::ExplorationContext>,
) {
    if !crate::search::tunables::metrics_enabled() {
        return;
    }
    tokio::task::block_in_place(|| {
        let metrics_ctx = crate::metrics::MetricsContext::resolve(app);
        crate::metrics::account_mcp_response(
            &app.db,
            chars as i64,
            &mcp_harness(conn),
            tool_name,
            session_id,
            &metrics_ctx,
            exploration,
        );
    });
}

fn request_tool_name(request: &JsonRpcRequest) -> Option<&str> {
    if request.method != "tools/call" {
        return None;
    }
    request.params.get("name").and_then(|v| v.as_str())
}

/// Extract the `turn_id` and `area` arguments from a `code_map_write` or
/// `code_map_load` tool call request and determine `map_status` from the tool
/// result. Returns `None` for all other methods and tool names.
/// `code_map_status` is read-only and lightweight — it does not emit
/// exploration attribution rows.
fn request_exploration_context(
    request: &JsonRpcRequest,
    tool_result: Option<&serde_json::Value>,
) -> Option<crate::metrics::ExplorationContext> {
    if request.method != "tools/call" {
        return None;
    }
    let tool_name = request.params.get("name").and_then(|v| v.as_str())?;
    if !matches!(tool_name, "code_map_write" | "code_map_load") {
        return None;
    }
    let args = request.params.get("arguments")?;
    let turn_id = args
        .get("turn_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let area = args
        .get("area")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // map_status: for code_map_write it is always a miss (writing a new/updated
    // map). For code_map_load: only a found+fresh map is a hit; stale,
    // rescout-required, absent, or malformed results are misses.
    let map_status = if tool_name == "code_map_write" {
        Some(crate::db::metrics::MapStatus::Miss)
    } else {
        tool_result.map(|v| {
            let found = v.get("found").and_then(|v| v.as_bool()).unwrap_or(false);
            let fresh = v
                .get("freshness")
                .and_then(|f| f.get("verdict"))
                .and_then(|v| v.as_str())
                == Some("fresh");
            if found && fresh {
                crate::db::metrics::MapStatus::Hit
            } else {
                crate::db::metrics::MapStatus::Miss
            }
        })
    };

    Some(crate::metrics::ExplorationContext {
        turn_id,
        area,
        map_status,
    })
}

/// Run the MCP server loop, reading JSON-RPC from stdin, writing to stdout.
pub async fn run_server(app: Arc<App>) -> Result<(), MemoryError> {
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    run_server_io(app, stdin, stdout).await
}

pub async fn run_server_io<R, W>(app: Arc<App>, reader: R, writer: W) -> Result<(), MemoryError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // In-process stdio backend: dispatch synchronously against the local
    // `App`, offloaded via `block_in_place` so blocking tool work doesn't
    // stall the tokio reactor. A later shared-daemon backend swaps this
    // closure for a channel round-trip to a single-owner dispatcher.
    run_framing_loop(&app, reader, writer, |request| {
        tokio::task::block_in_place(|| dispatch(&app, request))
    })
    .await
}

/// Per-connection MCP framing loop: read newline-delimited JSON-RPC requests
/// from `reader`, hand each one to `dispatch_fn` to obtain a response, write
/// the response to `writer`, and account response metrics. `dispatch_fn` is
/// the dispatch backend — today an in-process synchronous call (see
/// `run_server_io`), and in a future shared-daemon transport a channel
/// round-trip to a single-owner dispatcher. Metrics accounting needs access
/// to `app` (for the DB + process-global collab/task-tag context) and a
/// per-connection `ConnectionContext` (for harness/session attribution learned
/// from *this* connection's own `initialize` — see `ConnectionContext` for
/// why that must not live on `App`), plus the original request (for tool name
/// / session id / exploration context), independent of how the response was
/// obtained.
async fn run_framing_loop<R, W, F>(
    app: &Arc<App>,
    reader: R,
    writer: W,
    mut dispatch_fn: F,
) -> Result<(), MemoryError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(&JsonRpcRequest) -> Option<JsonRpcResponse>,
{
    let mut stdout = writer;
    let mut lines = reader.lines();
    let mut conn = ConnectionContext::new();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(None, -32700, &format!("Parse error: {e}"));
                let chars = write_response(&mut stdout, &resp).await?;
                account_response_metrics(app, &conn, chars, None, conn.session_id.as_deref(), None);
                continue;
            }
        };

        if request.jsonrpc != "2.0" {
            let resp = JsonRpcResponse::error(
                request.id.clone(),
                -32600,
                "Invalid Request: jsonrpc must be '2.0'",
            );
            let chars = write_response(&mut stdout, &resp).await?;
            account_response_metrics(app, &conn, chars, None, conn.session_id.as_deref(), None);
            continue;
        }

        // Connection-local attribution: learn session id / harness from this
        // connection's own `initialize` request before dispatching it. Kept
        // in the framing loop (rather than inside `dispatch`) so `dispatch`'s
        // signature — and its many direct test callers — stay untouched; the
        // framing loop already owns the parsed `request` and is the one place
        // that is per-connection by construction.
        if request.method == "initialize" {
            conn.learn(&request.params);
        }

        let response = dispatch_fn(&request);

        if let Some(resp) = response {
            // Extract the tool result JSON (if this is a successful tools/call)
            // so code-map tools can determine map_hit vs map_miss from `found`.
            let tool_result_json: Option<serde_json::Value> = resp
                .result
                .as_ref()
                .and_then(|r| r.get("content"))
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|item| item.get("text"))
                .and_then(|t| t.as_str())
                .and_then(|s| serde_json::from_str(s).ok());
            let exploration = request_exploration_context(&request, tool_result_json.as_ref());
            let chars = write_response(&mut stdout, &resp).await?;
            let sid = conn
                .session_id
                .clone()
                .or_else(|| request_collab_session_id(&request));
            account_response_metrics(
                app,
                &conn,
                chars,
                request_tool_name(&request),
                sid.as_deref(),
                exploration.as_ref(),
            );
        }
    }

    Ok(())
}

async fn write_response(
    stdout: &mut (impl AsyncWrite + Unpin),
    resp: &JsonRpcResponse,
) -> Result<usize, MemoryError> {
    let json = serde_json::to_string(resp)?;
    let chars = json.len();
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(chars)
}

pub fn dispatch(app: &App, request: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    let id = request.id.clone();

    match request.method.as_str() {
        // Metrics attribution (harness/session id) is learned per-connection
        // in `run_framing_loop`'s `ConnectionContext`, not here — `dispatch`
        // has no notion of "which connection" once a single `App` is shared
        // across many (see `ConnectionContext` doc comment). `dispatch` stays
        // a pure request -> response function so its many direct test callers
        // (outside this module) are unaffected by this change.
        "initialize" => Some(JsonRpcResponse::success(
            id,
            protocol::capabilities_response(),
        )),

        "tools/list" => {
            let tool_list = tools::tool_definitions(app);
            Some(JsonRpcResponse::success(
                id,
                serde_json::json!({ "tools": tool_list }),
            ))
        }

        "tools/call" => {
            let tool_name = request.params.get("name").and_then(|v| v.as_str());
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            match tool_name {
                Some(name) => {
                    let result = tools::call_tool(app, name, &arguments);
                    match result {
                        Ok(content) => Some(JsonRpcResponse::success(
                            id,
                            serde_json::json!({
                                "content": [{
                                    "type": "text",
                                    "text": serde_json::to_string_pretty(&content).unwrap_or_default()
                                }]
                            }),
                        )),
                        Err(e) => {
                            tracing::error!(request_id = ?id, "Tool error in {}: {}", name, e);
                            let user_message = match &e {
                                MemoryError::Validation(msg) => msg.clone(),
                                MemoryError::NotFound(msg) => msg.clone(),
                                MemoryError::Permission(msg) => msg.clone(),
                                MemoryError::Json(err) => format!("invalid JSON: {err}"),
                                MemoryError::Config(msg) => format!("config error: {msg}"),
                                _ => "Internal server error".to_string(),
                            };
                            Some(JsonRpcResponse::success(
                                id,
                                serde_json::json!({
                                    "content": [{
                                        "type": "text",
                                        "text": serde_json::json!({"error": user_message}).to_string()
                                    }],
                                    "isError": true
                                }),
                            ))
                        }
                    }
                }
                None => Some(JsonRpcResponse::error(id, -32602, "Missing tool name")),
            }
        }

        "notifications/initialized" | "notifications/cancelled" => None, // No response

        _ => Some(JsonRpcResponse::error(
            id,
            -32601,
            &format!("Unknown method: {}", request.method),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn run_with_input(input: &str) -> String {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);

        client_in.write_all(input.as_bytes()).await.unwrap();
        client_in.shutdown().await.unwrap();

        run_server_io(app, BufReader::new(server_in), server_out)
            .await
            .unwrap();

        let mut output = String::new();
        client_out.read_to_string(&mut output).await.unwrap();
        output
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_json_returns_parse_error() {
        let output = run_with_input("{not json}\n").await;
        assert!(output.contains("\"code\":-32700"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fragmented_valid_request_is_handled() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);

        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,")
            .await
            .unwrap();
        client_in
            .write_all(b"\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();

        run_server_io(app, BufReader::new(server_in), server_out)
            .await
            .unwrap();

        let mut output = String::new();
        client_out.read_to_string(&mut output).await.unwrap();
        assert!(output.contains("\"protocolVersion\":\"2024-11-05\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pretty_printed_multiline_json_yields_parse_errors_without_crashing() {
        let output = run_with_input(
            "{\n  \"jsonrpc\":\"2.0\",\n  \"id\":1,\n  \"method\":\"initialize\"\n}\n",
        )
        .await;
        assert!(output.contains("\"code\":-32700"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn notifications_do_not_emit_responses() {
        let output = run_with_input(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n",
        )
        .await;
        assert!(output.is_empty());
    }

    use crate::metrics::METRICS_ENV_LOCK;

    // Holds the crate-wide metrics env lock across `.await` to serialize the
    // `IRONMEM_METRICS` kill switch; the guard must outlive the server run.
    // The lock is shared with `search::tunables` tests so neither module can
    // clobber the other's `IRONMEM_METRICS` mutation under the parallel runner.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn write_response_records_mcp_response_token_usage() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let mcp: Vec<_> = rows.iter().filter(|r| r.source == "mcp_response").collect();
        assert_eq!(mcp.len(), 1, "exactly one mcp_response row");
        assert!(mcp[0].estimated);
        assert!(mcp[0].chars > 0);
        assert_eq!(mcp[0].input_tokens, 0);
        assert_eq!(
            mcp[0].output_tokens,
            crate::metrics::estimate_tokens(mcp[0].chars)
        );
        assert_eq!(mcp[0].harness, "claude");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_json_records_mcp_response_metric() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in.write_all(b"{not json}\n").await.unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("\"code\":-32700"));

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert_eq!(
            rows.iter()
                .filter(|r| r.source == "mcp_response" && r.estimated)
                .count(),
            1
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_request_records_mcp_response_metric() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(b"{\"jsonrpc\":\"1.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();
        assert!(out.contains("\"code\":-32600"));

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert_eq!(
            rows.iter()
                .filter(|r| r.source == "mcp_response" && r.estimated)
                .count(),
            1
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn non_ascii_response_metric_matches_bytes_written() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"caf\xc3\xa9\",\"params\":{}}\n")
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();
        let emitted = out.trim_end();
        assert!(emitted.len() > emitted.chars().count());

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let mcp: Vec<_> = rows.iter().filter(|r| r.source == "mcp_response").collect();
        assert_eq!(mcp.len(), 1, "exactly one mcp_response row");
        assert_eq!(mcp[0].chars, emitted.len() as i64);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_client_info_can_attribute_codex_harness() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"codex-cli\",\"version\":\"1.0.0\"}}}\n",
            )
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let mcp: Vec<_> = rows.iter().filter(|r| r.source == "mcp_response").collect();
        assert_eq!(mcp.len(), 1, "exactly one mcp_response row");
        assert_eq!(mcp[0].harness, "codex");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_session_id_is_sanitized_before_summary_accumulation() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"sessionId\":\"../bad-session\"}}\n",
            )
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();

        assert!(app
            .db
            .get_session_summary("../bad-session")
            .unwrap()
            .is_none());
        let s = app
            .db
            .get_session_summary("bad-session")
            .unwrap()
            .expect("sanitized session summary exists");
        assert!(s.mcp_chars_served > 0);
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn kill_switch_suppresses_mcp_response_rows() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_METRICS", "0");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();
        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert!(rows.iter().all(|r| r.source != "mcp_response"));
        std::env::remove_var("IRONMEM_METRICS");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn collab_call_accumulates_mcp_chars_preserving_hook_fields() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    "collab-xyz",
                    "/tmp/repo",
                    "main",
                    Some("task"),
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();
        let seeded = crate::db::metrics::SessionSummary {
            session_id: "collab-xyz".to_string(),
            harness: "claude".to_string(),
            workspace_root: None,
            started_at: Some("2026-06-11T00:00:00Z".to_string()),
            ended_at: None,
            peak_occupancy_pct: Some(0.42),
            total_input_tokens: 1234,
            total_output_tokens: 567,
            mcp_chars_served: 0,
            compactions: 3,
        };
        app.db.upsert_session_summary(&seeded).unwrap();

        let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"collab_status\",\"arguments\":{\"session_id\":\"collab-xyz\"}}}\n";
        let (mut client_in, server_in) = tokio::io::duplex(8192);
        let (server_out, mut client_out) = tokio::io::duplex(8192);
        client_in.write_all(req).await.unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();

        let s = app.db.get_session_summary("collab-xyz").unwrap().unwrap();
        assert!(s.mcp_chars_served > 0, "mcp_chars accumulated");
        assert_eq!(s.peak_occupancy_pct, Some(0.42));
        assert_eq!(s.total_input_tokens, 1234);
        assert_eq!(s.total_output_tokens, 567);
        assert_eq!(s.compactions, 3);
        assert_eq!(s.started_at.as_deref(), Some("2026-06-11T00:00:00Z"));
        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let mcp = rows
            .iter()
            .find(|r| r.source == "mcp_response")
            .expect("collab_status response row recorded");
        assert_eq!(mcp.tool_name.as_deref(), Some("collab_status"));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_response_row_is_stamped_with_active_collab_session() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        // Seed a collab session and set it as active.
        let sid = "server-test-collab-session";
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    sid,
                    "/tmp/repo",
                    "main",
                    None,
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();
        app.set_active_collab_session(sid);

        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let mcp: Vec<_> = rows.iter().filter(|r| r.source == "mcp_response").collect();
        assert_eq!(mcp.len(), 1, "exactly one mcp_response row");
        assert_eq!(
            mcp[0].collab_session_id.as_deref(),
            Some(sid),
            "row must carry the active collab session id"
        );
        assert_eq!(
            mcp[0].collab_phase.as_deref(),
            Some("planning"),
            "fresh session (PlanParallelDrafts) → 'planning' bucket"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn non_collab_tool_session_id_arg_does_not_create_summary() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"status\",\"arguments\":{\"session_id\":\"spoof\"}}}\n";
        let (mut client_in, server_in) = tokio::io::duplex(8192);
        let (server_out, mut client_out) = tokio::io::duplex(8192);
        client_in.write_all(req).await.unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out = String::new();
        client_out.read_to_string(&mut out).await.unwrap();

        assert!(app.db.get_session_summary("spoof").unwrap().is_none());
    }

    /// Task 2 acceptance test: two sequential connections through ONE shared
    /// `App`, each with a distinct `clientInfo`/session, must each get their
    /// own attribution — the second connection's `initialize` must not be
    /// silently dropped in favor of the first's (the pre-fix behavior, when
    /// `App::session_id`/`App::harness` were process-global "set once" cells).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn sequential_connections_on_shared_app_get_independent_attribution() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        // Connection 1: claude-code client, session "session-one". Second
        // request is a cheap `tools/call` (not `tools/list`, whose full tool
        // catalog can exceed the duplex buffer and deadlock: the framing loop
        // would block writing a response nobody drains until after
        // `run_server_io` returns).
        let (mut client_in, server_in) = tokio::io::duplex(65536);
        let (server_out, mut client_out) = tokio::io::duplex(65536);
        client_in
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"sessionId\":\"session-one\",\"clientInfo\":{\"name\":\"claude-code\",\"version\":\"1.0.0\"}}}\n\
                  {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"status\",\"arguments\":{}}}\n",
            )
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in), server_out)
            .await
            .unwrap();
        let mut out1 = String::new();
        client_out.read_to_string(&mut out1).await.unwrap();
        assert!(out1.contains("\"protocolVersion\""));

        // Connection 2, on the SAME `App`: codex-cli client, session
        // "session-two". If attribution were still process-global, this
        // connection's `initialize` would be ignored (guard already `Some`
        // from connection 1) and both of its responses below would be
        // mis-attributed to connection 1's harness/session forever.
        let (mut client_in2, server_in2) = tokio::io::duplex(65536);
        let (server_out2, mut client_out2) = tokio::io::duplex(65536);
        client_in2
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"sessionId\":\"session-two\",\"clientInfo\":{\"name\":\"codex-cli\",\"version\":\"1.0.0\"}}}\n\
                  {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"status\",\"arguments\":{}}}\n",
            )
            .await
            .unwrap();
        client_in2.shutdown().await.unwrap();
        run_server_io(Arc::clone(&app), BufReader::new(server_in2), server_out2)
            .await
            .unwrap();
        let mut out2 = String::new();
        client_out2.read_to_string(&mut out2).await.unwrap();
        assert!(out2.contains("\"protocolVersion\""));

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let mcp: Vec<_> = rows.iter().filter(|r| r.source == "mcp_response").collect();
        assert_eq!(
            mcp.len(),
            4,
            "2 responses (initialize + tools/call status) per connection x 2 connections"
        );

        let conn1: Vec<_> = mcp
            .iter()
            .filter(|r| r.session_id.as_deref() == Some("session-one"))
            .collect();
        let conn2: Vec<_> = mcp
            .iter()
            .filter(|r| r.session_id.as_deref() == Some("session-two"))
            .collect();
        assert_eq!(
            conn1.len(),
            2,
            "both of connection 1's responses attributed to session-one"
        );
        assert_eq!(
            conn2.len(),
            2,
            "both of connection 2's responses attributed to session-two, \
             not blocked by connection 1's already-learned session id"
        );
        assert!(
            conn1.iter().all(|r| r.harness == "claude"),
            "connection 1 attributed to the claude harness"
        );
        assert!(
            conn2.iter().all(|r| r.harness == "codex"),
            "connection 2 attributed to the codex harness, not clobbered by \
             connection 1's already-learned harness"
        );
    }
}
