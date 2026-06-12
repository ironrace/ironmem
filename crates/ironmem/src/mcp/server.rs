//! MCP server — JSON-RPC 2.0 over stdio.

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::app::App;
use super::protocol::{self, JsonRpcRequest, JsonRpcResponse};
use super::tools;
use crate::error::MemoryError;

fn mcp_harness() -> String {
    match std::env::var("IRONMEM_HARNESS").ok().as_deref() {
        Some("codex") => "codex".to_string(),
        _ => "claude".to_string(),
    }
}

/// Collab tool calls carry a `session_id` argument; use it as the D1 fallback
/// key for `mcp_chars_served` when no harness session id was learned.
fn request_collab_session_id(request: &JsonRpcRequest) -> Option<String> {
    if request.method != "tools/call" {
        return None;
    }
    request
        .params
        .get("arguments")
        .and_then(|a| a.get("session_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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
    let mut stdout = writer;
    let mut lines = reader.lines();
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
                if crate::search::tunables::metrics_enabled() {
                    crate::metrics::account_mcp_response(
                        &app.db,
                        chars as i64,
                        &mcp_harness(),
                        app.session_id_snapshot().as_deref(),
                    );
                }
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
            if crate::search::tunables::metrics_enabled() {
                crate::metrics::account_mcp_response(
                    &app.db,
                    chars as i64,
                    &mcp_harness(),
                    app.session_id_snapshot().as_deref(),
                );
            }
            continue;
        }

        // Run synchronous tool dispatch without blocking the tokio reactor.
        // block_in_place yields the current thread to the runtime for other async
        // tasks while executing the blocking work inline (no Send requirement).
        let response = tokio::task::block_in_place(|| dispatch(&app, &request));

        if let Some(resp) = response {
            let chars = write_response(&mut stdout, &resp).await?;
            if crate::search::tunables::metrics_enabled() {
                let sid = app
                    .session_id_snapshot()
                    .or_else(|| request_collab_session_id(&request));
                crate::metrics::account_mcp_response(
                    &app.db,
                    chars as i64,
                    &mcp_harness(),
                    sid.as_deref(),
                );
            }
        }
    }

    Ok(())
}

async fn write_response(
    stdout: &mut (impl AsyncWrite + Unpin),
    resp: &JsonRpcResponse,
) -> Result<usize, MemoryError> {
    let json = serde_json::to_string(resp)?;
    let chars = json.chars().count();
    stdout.write_all(json.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(chars)
}

pub fn dispatch(app: &App, request: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            app.learn_session_id(&request.params);
            Some(JsonRpcResponse::success(
                id,
                protocol::capabilities_response(),
            ))
        }

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
                            tracing::error!("Tool error in {}: {}", name, e);
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
        assert!(mcp[0].harness == "claude" || mcp[0].harness == "codex");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn kill_switch_suppresses_mcp_response_rows() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_METRICS", "0");
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
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
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

        let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"status\",\"arguments\":{\"session_id\":\"collab-xyz\"}}}\n";
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
    }
}
