//! MCP server — JSON-RPC 2.0 over stdio.

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::app::{harness_from_client_info, session_id_from_params, App};
use super::protocol::{self, JsonRpcRequest, JsonRpcResponse};
use super::tools;
use crate::error::MemoryError;

/// Which kind of connection a framing loop is serving. Controls whether the
/// process's own `IRONMEM_SESSION_ID`/`IRONMEM_HARNESS` env vars are honored
/// as an attribution override (H4).
///
/// Those overrides are meaningful only for a direct single-client stdio
/// `serve`: there the process itself IS the one client, so its env is exactly
/// that client's identity. Once a single daemon process accepts many
/// connections (`serve --listen`/`serve_accept_loop`), the daemon's OWN env
/// — inherited from whichever process happened to spawn it first — must never
/// be forced onto every OTHER connection; each connection's attribution must
/// come purely from its own `initialize` request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TransportMode {
    /// Direct single-client stdio `serve`: env overrides are honored.
    Stdio,
    /// A daemon-accepted connection: env overrides are ignored; attribution
    /// comes only from this connection's own `initialize`.
    DaemonConnection,
}

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
    /// Whether the `IRONMEM_SESSION_ID`/`IRONMEM_HARNESS` env overrides apply
    /// to this connection (H4) — true only for `TransportMode::Stdio`.
    honor_env: bool,
}

impl ConnectionContext {
    /// Seed from the `IRONMEM_SESSION_ID` env override when `mode` honors env
    /// (`TransportMode::Stdio`), matching the pre-existing `App::session_id`
    /// construction-time seed. An env value takes priority over anything
    /// `initialize` supplies (`learn` never overwrites an already-`Some`
    /// value), exactly mirroring prior stdio behavior. A daemon-connection
    /// never seeds from env: attribution comes solely from `learn`.
    fn new(mode: TransportMode) -> Self {
        let honor_env = matches!(mode, TransportMode::Stdio);
        Self {
            session_id: honor_env
                .then(|| std::env::var("IRONMEM_SESSION_ID").ok())
                .flatten(),
            harness: None,
            honor_env,
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
    if ctx.honor_env {
        if let Ok(value) = std::env::var("IRONMEM_HARNESS") {
            if let Some(id) = crate::harness::canonicalize_input(&value, crate::harness::REGISTRY) {
                return id.to_string();
            }
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

/// A request being dispatched, paired with the request itself so the framing
/// loop can account metrics and release the ordering barrier when it lands.
///
/// Boxed because the loop pushes from two sites — newly read, and released
/// from the mutation queue — and two `async` blocks are two distinct opaque
/// types that cannot share one `FuturesUnordered`. `LocalBoxFuture`, not
/// `BoxFuture`: `Arc<App>` is `!Send` (see `daemon`'s module doc).
type InFlightRequest<'a> =
    futures_util::future::LocalBoxFuture<'a, (JsonRpcRequest, Option<JsonRpcResponse>)>;

fn dispatch_in_flight<'a>(app: &'a Arc<App>, request: JsonRpcRequest) -> InFlightRequest<'a> {
    Box::pin(async move {
        let response = dispatch_request(app, &request).await;
        (request, response)
    })
}

/// Whether this request persists state, and so must hold its place in the
/// per-connection ordering barrier. Derived from `tools::MUTATING_TOOLS` — the
/// same list that drives read-only mode gating — so the two cannot disagree
/// about what a "write" is.
fn is_mutating_request(request: &JsonRpcRequest) -> bool {
    request.method == "tools/call"
        && request_tool_name(request).is_some_and(tools::is_mutating_tool)
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
    run_framing_loop(&app, reader, writer, TransportMode::Stdio).await
}

/// Daemon-connection variant of [`run_server_io`] (H4): identical framing
/// loop, but `TransportMode::DaemonConnection` means this connection's
/// `ConnectionContext` never seeds from — or overrides with — the daemon
/// process's own `IRONMEM_SESSION_ID`/`IRONMEM_HARNESS` env vars. Used
/// exclusively by `mcp::daemon::serve_accept_loop`, where a single daemon
/// process serves many independent connections and the process env belongs
/// to whichever client happened to spawn it first, not to every connection.
pub(crate) async fn run_server_io_daemon_connection<R, W>(
    app: Arc<App>,
    reader: R,
    writer: W,
) -> Result<(), MemoryError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    run_framing_loop(&app, reader, writer, TransportMode::DaemonConnection).await
}

/// How many requests one connection may have in flight at once.
///
/// Backpressure, not a throughput target: the loop simply stops reading new
/// requests at this depth until one completes, so a client that pipelines
/// without bound cannot make the server buffer without bound. Requests park
/// here only while awaiting readiness — `dispatch` itself is still serialized
/// — so the depth needed is "writes a client may have outstanding during
/// warm-up", not "concurrent CPU work".
const MAX_IN_FLIGHT_REQUESTS: usize = 64;

/// How many mutations may sit in the per-connection ordering queue.
///
/// Mutations are serialized (see `run_framing_loop`), so a client that
/// pipelines writes during warm-up builds a backlog here rather than in
/// `in_flight`. Bounding it bounds the memory one connection can pin: request
/// bodies are capped at 100k chars, so this is the difference between a few MB
/// and unbounded. Exceeding it is answered with an explicit error rather than
/// by stalling the reader — a stall would silently re-create the head-of-line
/// blocking this loop exists to avoid, and would be far harder to diagnose.
const MAX_QUEUED_MUTATIONS: usize = 64;

/// Per-connection MCP framing loop: read newline-delimited JSON-RPC requests
/// from `reader`, dispatch each one, write the response to `writer`, and
/// account response metrics.
///
/// Requests are pipelined, but only READS may overtake. That matters because
/// one MCP client is one connection — without pipelining, a warm-up
/// `add_drawer` would stall a following `search` on the *same* connection for
/// the whole readiness timeout, which is precisely the stall the gate exists
/// to avoid.
///
/// Mutations (`tools::MUTATING_TOOLS`) are held to their arrival order and run
/// one at a time. Letting them overtake would corrupt state, not just reorder
/// replies: only the three embedder-dependent tools park on the readiness
/// gate, so an unordered `delete_drawer` would execute and commit while the
/// `add_drawer` before it was still parked, and the add would then re-create
/// the row the client asked to delete. Reads are unaffected and still overtake
/// freely, which is the whole point of the pipeline.
///
/// Responses are therefore written out of request order for reads; clients
/// match responses to requests by `id`, so order is not significant.
///
/// Concurrency here is limited to await points. `dispatch` runs inside
/// `block_in_place` and this whole loop is `!Send` (see `daemon`'s module
/// doc), so at most one `dispatch` ever runs at a time and `App`'s
/// single-owner invariant is preserved. `MAX_IN_FLIGHT_REQUESTS` bounds the
/// dispatched set and `MAX_QUEUED_MUTATIONS` the ordering backlog. Because a
/// mutation occupies at most ONE `in_flight` slot no matter how many are
/// queued, a pipelined burst of writes cannot exhaust the in-flight budget and
/// lock the reader out — read admission is preserved by construction.
///
/// `mode` (H4) controls whether this connection's `ConnectionContext` honors
/// the `IRONMEM_SESSION_ID`/`IRONMEM_HARNESS` env overrides — see
/// `TransportMode`. Metrics accounting needs access to `app` (for the DB +
/// process-global collab/task-tag context) and a per-connection
/// `ConnectionContext` (for harness/session attribution learned from *this*
/// connection's own `initialize` — see `ConnectionContext` for why that must
/// not live on `App`), plus the original request (for tool name / session id /
/// exploration context), independent of how the response was obtained.
async fn run_framing_loop<R, W>(
    app: &Arc<App>,
    reader: R,
    writer: W,
    mode: TransportMode,
) -> Result<(), MemoryError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};
    use std::collections::VecDeque;

    let mut stdout = writer;
    let mut lines = reader.lines();
    let mut conn = ConnectionContext::new(mode);
    let mut in_flight = FuturesUnordered::new();
    let mut reader_done = false;
    // Ordering barrier: mutations run strictly FIFO, one at a time.
    let mut queued_mutations: VecDeque<JsonRpcRequest> = VecDeque::new();
    let mut mutation_in_flight = false;

    loop {
        // Only the dispatched set is capped here. Queued mutations are bounded
        // separately, so a burst of writes can never starve read admission.
        let may_read = !reader_done && in_flight.len() < MAX_IN_FLIGHT_REQUESTS;

        tokio::select! {
            // Biased toward draining: prefer answering work already accepted
            // over accepting more of it.
            biased;

            Some((request, response)) = in_flight.next() => {
                if is_mutating_request(&request) {
                    mutation_in_flight = false;
                }
                write_and_account(app, &conn, &mut stdout, &request, response).await?;

                // Release the next queued mutation, preserving arrival order.
                if !mutation_in_flight {
                    if let Some(next) = queued_mutations.pop_front() {
                        mutation_in_flight = true;
                        in_flight.push(dispatch_in_flight(app, next));
                    }
                }
            }

            line = lines.next_line(), if may_read => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        reader_done = true;
                        continue;
                    }
                    // A read error is NOT a clean close: anything the client
                    // already sent and we have not parsed is lost, and
                    // pipelining means that can be a whole batch rather than
                    // one request. Say so, and surface it to the caller so the
                    // daemon logs an error close rather than a normal one.
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            in_flight = in_flight.len(),
                            queued_mutations = queued_mutations.len(),
                            "connection read failed; unread requests are abandoned"
                        );
                        return Err(MemoryError::Io(e));
                    }
                };
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }

                let request: JsonRpcRequest = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        let resp =
                            JsonRpcResponse::error(None, -32700, &format!("Parse error: {e}"));
                        let chars = write_response(&mut stdout, &resp).await?;
                        account_response_metrics(
                            app, &conn, chars, None, conn.session_id.as_deref(), None,
                        );
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
                    account_response_metrics(
                        app, &conn, chars, None, conn.session_id.as_deref(), None,
                    );
                    continue;
                }

                // Connection-local attribution: learn session id / harness from
                // this connection's own `initialize` request before dispatching
                // it. Kept in the framing loop (rather than inside `dispatch`)
                // so `dispatch`'s signature — and its many direct test callers —
                // stay untouched; the framing loop already owns the parsed
                // `request` and is the one place that is per-connection by
                // construction. Done at read time, so it is ordered against the
                // requests behind it even though dispatch is pipelined.
                if request.method == "initialize" {
                    conn.learn(&request.params);
                }

                // Mutations queue behind any mutation already running, so their
                // arrival order is their execution order. Reads bypass this
                // entirely.
                if is_mutating_request(&request) {
                    if mutation_in_flight {
                        if queued_mutations.len() >= MAX_QUEUED_MUTATIONS {
                            let resp = tool_error_response(
                                request.id.clone(),
                                request_tool_name(&request),
                                MemoryError::Validation(format!(
                                    "too many writes queued on this connection \
                                     ({MAX_QUEUED_MUTATIONS}); retry once earlier \
                                     writes complete"
                                )),
                            );
                            let chars = write_response(&mut stdout, &resp).await?;
                            account_response_metrics(
                                app, &conn, chars, request_tool_name(&request),
                                conn.session_id.as_deref(), None,
                            );
                            continue;
                        }
                        queued_mutations.push_back(request);
                        continue;
                    }
                    mutation_in_flight = true;
                }

                in_flight.push(dispatch_in_flight(app, request));
            }

            else => break,
        }

        if reader_done && in_flight.is_empty() && queued_mutations.is_empty() {
            break;
        }
    }

    Ok(())
}

/// Write one dispatched response and account its metrics. Split out of
/// `run_framing_loop` so the pipelined completion branch stays readable.
async fn write_and_account(
    app: &Arc<App>,
    conn: &ConnectionContext,
    stdout: &mut (impl AsyncWrite + Unpin),
    request: &JsonRpcRequest,
    response: Option<JsonRpcResponse>,
) -> Result<(), MemoryError> {
    let Some(resp) = response else {
        return Ok(());
    };
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
    let exploration = request_exploration_context(request, tool_result_json.as_ref());
    let chars = write_response(stdout, &resp).await?;
    let sid = conn
        .session_id
        .clone()
        .or_else(|| request_collab_session_id(request));
    account_response_metrics(
        app,
        conn,
        chars,
        request_tool_name(request),
        sid.as_deref(),
        exploration.as_ref(),
    );
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

/// Write-shaped tools must wait for readiness, but `App` is confined to a
/// single owner: every `dispatch` runs synchronously inside `block_in_place`,
/// on the one thread that owns the `App`. Waiting *inside* a handler therefore
/// parks that thread, which stalls not just this connection's later requests
/// but every other connection the daemon is serving. So the wait happens here,
/// asynchronously, before entering `dispatch`; once ready, the normal
/// synchronous handler remains serialized as before. Anything that can be
/// rejected without readiness is rejected before the wait, and the wait itself
/// consumes no thread — see `tools::precheck_write_request` and
/// `ReadinessGate::wait_for_write_async`.
///
/// Applies to BOTH transports. A bare stdio `serve` is one client on one
/// connection, so a write parked inside its handler would head-of-line block
/// that client's own reads just as surely — and stdio is the transport a
/// harness uses directly.
async fn dispatch_request(app: &Arc<App>, request: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    let tool_name = request.params.get("name").and_then(|value| value.as_str());
    let is_write =
        request.method == "tools/call" && tool_name.is_some_and(tools::is_write_shaped_tool);

    if is_write && app.is_warming_up() {
        // Reject what does not depend on readiness — unknown tool, mode
        // gating, malformed arguments — before parking this request on the
        // gate. Otherwise a single malformed call blocks for the full
        // readiness timeout and is then rejected anyway.
        let arguments = request
            .params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        if let Some(name) = tool_name {
            if let Err(error) =
                tokio::task::block_in_place(|| tools::precheck_write_request(app, name, &arguments))
            {
                return Some(tool_error_response(request.id.clone(), tool_name, error));
            }
        }

        // `wait_for_write_async`, not `spawn_blocking(wait_for_write)`: the
        // number of concurrently warming-up writes is bounded only by the
        // number of connected clients, and one blocking-pool thread per
        // waiter would starve every other `spawn_blocking` user in the
        // process for the length of the readiness timeout.
        let timeout = app.config.write_readiness_timeout();
        if let Err(error) = app.memory_ready.wait_for_write_async(timeout).await {
            return Some(tool_error_response(request.id.clone(), tool_name, error));
        }
    }

    tokio::task::block_in_place(|| dispatch(app, request))
}

fn tool_error_response(
    id: Option<serde_json::Value>,
    tool_name: Option<&str>,
    error: MemoryError,
) -> JsonRpcResponse {
    tracing::error!(request_id = ?id, "Tool error in {}: {}", tool_name.unwrap_or("<unknown>"), error);
    let user_message = match &error {
        MemoryError::Validation(message)
        | MemoryError::NotFound(message)
        | MemoryError::Permission(message)
        | MemoryError::NotReady(message) => message.clone(),
        MemoryError::Json(error) => format!("invalid JSON: {error}"),
        MemoryError::Config(message) => format!("config error: {message}"),
        _ => "Internal server error".to_string(),
    };
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::json!({"error": user_message}).to_string()
            }],
            "isError": true
        }),
    )
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
                        Err(error) => Some(tool_error_response(id, Some(name), error)),
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
    use crate::mcp::readiness::ReadinessGate;
    use serde_json::json;
    use std::time::Duration;
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
    async fn daemon_warmup_write_wait_does_not_block_a_read_only_request() {
        // Reads the env-controlled readiness timeout; pin it so a test that
        // WRITES that var cannot change this test's bound mid-run.
        let _env = EnvGuard::pin(WRITE_READINESS_TIMEOUT_ENV);
        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let readiness = Arc::new(ReadinessGate::new_pending());
        Arc::get_mut(&mut app)
            .expect("test has the only App reference")
            .memory_ready = Arc::clone(&readiness);

        let write: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "add_drawer",
                "arguments": {"content": "queued write", "wing": "race"}
            }
        }))
        .unwrap();
        let search: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "queued"}}
        }))
        .unwrap();

        let write_wait = dispatch_request(&app, &write);
        tokio::pin!(write_wait);

        // Rust futures are lazy: constructing `write_wait` does NOT start the
        // write. Drive it here so it genuinely reaches its readiness await
        // point and is in flight for the rest of this test. It must still be
        // pending afterwards — the gate is unresolved, so a correct dispatch
        // cannot have produced a response yet. Without this step the test
        // would pass even if the readiness wait were serialized inside the
        // single-owner dispatcher, because no write would ever be in flight.
        let still_pending = tokio::time::timeout(Duration::from_millis(250), &mut write_wait).await;
        assert!(
            still_pending.is_err(),
            "warm-up write must still be waiting on the unresolved readiness gate, \
             got a completed response instead: {:?}",
            still_pending.ok().flatten()
        );

        // With a write genuinely parked on the readiness gate, a read-only
        // request must still be serviced. If that wait occupied the daemon's
        // single dispatcher, this search call would not complete before the
        // gate resolves.
        let search_response =
            tokio::time::timeout(Duration::from_millis(250), dispatch_request(&app, &search))
                .await
                .expect("read-only search must stay responsive while a write waits")
                .expect("search produces a response");
        assert_eq!(search_response.id, Some(json!(2)));
        assert_eq!(
            search_response.result.unwrap()["content"][0]["text"]
                .as_str()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                .and_then(|payload| payload["warming_up"].as_bool()),
            Some(true)
        );

        readiness.resolve_ready();
        let write_response = tokio::time::timeout(Duration::from_secs(1), &mut write_wait)
            .await
            .expect("write must resume when readiness resolves")
            .expect("write produces a response");
        assert_eq!(write_response.id, Some(json!(1)));
        assert_ne!(
            write_response.result.unwrap()["isError"].as_bool(),
            Some(true),
            "resolved write must not become an error"
        );
    }

    use crate::config::EnvGuard;

    /// The env var `Config::write_readiness_timeout()` reads fresh on every
    /// call. Every test here that sets OR merely depends on it goes through
    /// `EnvGuard`, which holds the one crate-wide `config::ENV_LOCK` — see
    /// that lock's doc for why a second, local mutex would have been useless.
    const WRITE_READINESS_TIMEOUT_ENV: &str = "IRONMEM_WRITE_READINESS_TIMEOUT_SECS";

    /// Feeds `requests` down one connection and returns the first response the
    /// server writes back, driving `run_framing_loop` inline.
    ///
    /// `run_framing_loop`'s future is `!Send` (`App` is `!Sync`), so it cannot
    /// be `tokio::spawn`ed. `select!` drives the loop and the response reader
    /// concurrently on this one task instead, which is also exactly how the
    /// daemon runs it.
    async fn first_response_from_connection(
        app: &Arc<App>,
        mode: TransportMode,
        requests: &[serde_json::Value],
    ) -> serde_json::Value {
        // Big enough to hold every request before the loop starts draining:
        // these writes happen up front, so a buffer smaller than the batch
        // would block the writer and deadlock the test rather than exercise
        // the server.
        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        for request in requests {
            let line = format!("{request}\n");
            client_in.write_all(line.as_bytes()).await.unwrap();
        }

        let mut loop_fut = Box::pin(run_framing_loop(
            app,
            BufReader::new(server_in),
            server_out,
            mode,
        ));
        let mut responses = BufReader::new(client_out).lines();

        tokio::select! {
            result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
            line = responses.next_line() => serde_json::from_str(
                &line.unwrap().expect("server must write a response"),
            )
            .expect("response must be valid JSON"),
        }
    }

    /// The headline claim of the readiness gate — reads stay serviceable while
    /// a write waits out warm-up — has to hold at the *connection* level, which
    /// is the only level a real MCP client can observe. One client is one
    /// connection, so if the framing loop awaits request N before it will even
    /// read request N+1, a warm-up `add_drawer` stalls a following `search` for
    /// the entire readiness timeout.
    ///
    /// Both transports are covered: `Stdio` is what Claude Code actually uses.
    /// The env override bounds how long this takes to FAIL — without it a
    /// regression parks on the 90s production default rather than reporting.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_services_a_read_while_a_write_waits_for_readiness() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "5");

        for mode in [TransportMode::Stdio, TransportMode::DaemonConnection] {
            #[allow(clippy::arc_with_non_send_sync)]
            let mut app = Arc::new(App::open_for_test().unwrap());
            let _readiness = force_warming_up(&mut app);

            // Write first, read second, down a single connection.
            let requests = [
                json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {
                        "name": "add_drawer",
                        "arguments": {"content": "queued write", "wing": "race"}
                    }
                }),
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "search", "arguments": {"query": "queued"}}
                }),
            ];

            let first = tokio::time::timeout(
                Duration::from_secs(2),
                first_response_from_connection(&app, mode, &requests),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{mode:?}: the read got no response while the write was parked on \
                     readiness — the connection is head-of-line blocked"
                )
            });

            assert_eq!(
                first["id"],
                json!(2),
                "{mode:?}: the read must be answered first; the write is still \
                 waiting on an unresolved gate"
            );
        }
    }

    /// Ordering companion to the test above. Pipelining must let READS overtake
    /// a parked write — it must NOT let one mutation overtake another.
    ///
    /// `WRITE_SHAPED_TOOLS` is only the three embedder-dependent tools, so
    /// every other mutating tool (`delete_drawer`, `kg_add`, `collab_send`, …)
    /// skips the readiness wait entirely. Without an ordering barrier a client
    /// that sends `add_drawer` then `delete_drawer` on one connection during
    /// warm-up gets the delete executed and committed first, and the parked add
    /// then re-creates the row the client asked to remove — a silent
    /// data-integrity inversion the pre-pipelining serial loop made impossible.
    ///
    /// Asserted as: while a write is parked on an unresolved gate, a following
    /// mutation produces NO response at all.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_does_not_let_a_mutation_overtake_a_parked_write() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let readiness = force_warming_up(&mut app);

        let (mut client_in, server_in) = tokio::io::duplex(8192);
        let (server_out, client_out) = tokio::io::duplex(8192);
        for request in [
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "add_drawer",
                    "arguments": {
                        "content": "v1", "wing": "race", "logical_key": "ordering-key"
                    }
                }
            }),
            // Mutating but NOT write-shaped: it never touches the gate, so
            // nothing but an explicit ordering barrier holds it back.
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "delete_drawer", "arguments": {"id": "a".repeat(64)}}
            }),
        ] {
            client_in
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }

        let mut loop_fut = Box::pin(run_framing_loop(
            &app,
            BufReader::new(server_in),
            server_out,
            TransportMode::DaemonConnection,
        ));
        let mut responses = BufReader::new(client_out).lines();

        let early = tokio::select! {
            result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
            line = responses.next_line() => Some(line.unwrap().unwrap_or_default()),
            _ = tokio::time::sleep(Duration::from_millis(400)) => None,
        };
        assert!(
            early.is_none(),
            "a mutation was executed while an earlier write was still parked on \
             the readiness gate — per-connection mutation order is not preserved. \
             Got: {early:?}"
        );

        // Once the gate resolves both run, and the earlier write goes first.
        readiness.resolve_ready();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let line = tokio::select! {
                result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                line = responses.next_line() => line.unwrap().expect("a response"),
            };
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            ids.push(value["id"].clone());
        }
        assert_eq!(
            ids,
            vec![json!(1), json!(2)],
            "mutations must be answered in the order they were received"
        );
    }

    /// A backpressure cap that counted every accepted request would re-create
    /// the very stall this loop exists to remove: an agent restoring state at
    /// session start fires a burst of writes and then a `search`, and once the
    /// cap is reached the loop stops READING, so the search is not even parsed
    /// until a write drains — up to the full readiness timeout.
    ///
    /// The fix is structural rather than a bigger number: mutations are
    /// serialized, so they occupy exactly ONE in-flight slot no matter how many
    /// are outstanding, and the backlog is bounded separately. This drives more
    /// writes than either cap and still demands the read come back.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_answers_a_read_behind_a_full_pipeline_of_writes() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        let write_count = MAX_IN_FLIGHT_REQUESTS + 1;
        let mut requests: Vec<serde_json::Value> = (0..write_count)
            .map(|i| {
                json!({
                    "jsonrpc": "2.0", "id": i, "method": "tools/call",
                    "params": {
                        "name": "add_drawer",
                        "arguments": {"content": format!("burst {i}"), "wing": "race"}
                    }
                })
            })
            .collect();
        // The read is last, behind every one of those writes.
        requests.push(json!({
            "jsonrpc": "2.0", "id": 9999, "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "burst"}}
        }));

        let first = tokio::time::timeout(
            Duration::from_secs(5),
            first_response_from_connection(&app, TransportMode::DaemonConnection, &requests),
        )
        .await
        .expect(
            "the read behind a full write pipeline got no response — the \
             backpressure cap has re-created head-of-line blocking",
        );

        assert_eq!(
            first["id"],
            json!(9999),
            "the read must be answered while every write is still parked"
        );
    }

    /// Installs a fresh, never-resolved `Pending` readiness gate so every
    /// write-shaped request has to contend with an open warm-up window.
    fn force_warming_up(app: &mut Arc<App>) -> Arc<ReadinessGate> {
        let readiness = Arc::new(ReadinessGate::new_pending());
        Arc::get_mut(app)
            .expect("test has the only App reference")
            .memory_ready = Arc::clone(&readiness);
        readiness
    }

    fn tool_call(id: i64, name: &str, arguments: serde_json::Value) -> JsonRpcRequest {
        serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }))
        .expect("request fixture must deserialize")
    }

    fn tool_error_text(response: &JsonRpcResponse) -> String {
        let result = response
            .result
            .as_ref()
            .expect("tool result must be present");
        assert_eq!(
            result["isError"].as_bool(),
            Some(true),
            "expected an error response, got {result}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .expect("content[0].text must be a string");
        let payload: serde_json::Value =
            serde_json::from_str(text).expect("tool response text must be valid JSON");
        payload["error"]
            .as_str()
            .expect("error payload must carry a string message")
            .to_string()
    }

    /// A write whose arguments are malformed does not depend on readiness to
    /// be rejected, so it must not serve out the whole
    /// `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` window (90s by default) first.
    /// The outer bound here is far below that default: if the readiness wait
    /// still runs first, this times out instead of returning.
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_invalid_write_is_rejected_without_waiting_for_readiness() {
        // Reads the env-controlled readiness timeout; pin it so a test that
        // WRITES that var cannot change this test's bound mid-run.
        let _env = EnvGuard::pin(WRITE_READINESS_TIMEOUT_ENV);
        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        // `content` is required by `add_drawer` and is absent here.
        let invalid = tool_call(1, "add_drawer", json!({ "wing": "race" }));

        let response =
            tokio::time::timeout(Duration::from_secs(2), dispatch_request(&app, &invalid))
                .await
                .expect("an invalid write must fail fast, not wait out the readiness timeout")
                .expect("tools/call produces a response");

        let message = tool_error_text(&response);
        assert!(
            message.contains("content is required"),
            "expected the validation error, got: {message}"
        );
        assert!(
            !message.contains("readiness") && !message.contains("timed out"),
            "invalid input must not be reported as a readiness failure: {message}"
        );
    }

    /// Same fast-fail requirement for a request rejected by mode gating:
    /// authorization does not depend on readiness either.
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_forbidden_write_is_rejected_without_waiting_for_readiness() {
        // Reads the env-controlled readiness timeout; pin it so a test that
        // WRITES that var cannot change this test's bound mid-run.
        let _env = EnvGuard::pin(WRITE_READINESS_TIMEOUT_ENV);
        #[allow(clippy::arc_with_non_send_sync)]
        let mut app =
            Arc::new(App::open_for_test_with_mode(crate::config::McpAccessMode::ReadOnly).unwrap());
        let _readiness = force_warming_up(&mut app);

        let forbidden = tool_call(1, "add_drawer", json!({ "content": "x", "wing": "race" }));

        let response =
            tokio::time::timeout(Duration::from_secs(2), dispatch_request(&app, &forbidden))
                .await
                .expect("a forbidden write must fail fast, not wait out the readiness timeout")
                .expect("tools/call produces a response");

        let message = tool_error_text(&response);
        assert!(
            !message.contains("readiness") && !message.contains("timed out"),
            "a forbidden write must not be reported as a readiness failure: {message}"
        );
    }

    /// Readiness waiters must not each consume a Tokio blocking-pool thread.
    /// The pool is a shared, bounded resource — every other `spawn_blocking`
    /// user in the process (and `tokio::fs`) queues behind it — so a warm-up
    /// window with many concurrent writes must not be able to occupy it.
    ///
    /// `max_blocking_threads(1)` makes that exposure observable at small
    /// scale: with one waiter per blocking thread, a single parked waiter is
    /// enough to starve everything else for the full readiness timeout.
    #[test]
    fn many_readiness_waiters_do_not_starve_the_blocking_pool() {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        // Reads the env-controlled readiness timeout; pin it so a test that
        // WRITES that var cannot change this test's bound mid-run.
        let _env = EnvGuard::pin(WRITE_READINESS_TIMEOUT_ENV);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");

        runtime.block_on(async {
            #[allow(clippy::arc_with_non_send_sync)]
            let mut app = Arc::new(App::open_for_test().unwrap());
            let _readiness = force_warming_up(&mut app);

            let requests: Vec<JsonRpcRequest> = (0..32)
                .map(|i| {
                    tool_call(
                        i,
                        "add_drawer",
                        json!({ "content": format!("queued write {i}"), "wing": "race" }),
                    )
                })
                .collect();
            let mut waiters: FuturesUnordered<_> = requests
                .iter()
                .map(|request| dispatch_request(&app, request))
                .collect();

            // Drive every write to its readiness await point. None can
            // complete: the gate is never resolved.
            assert!(
                tokio::time::timeout(Duration::from_millis(500), waiters.next())
                    .await
                    .is_err(),
                "no write can complete while the readiness gate is unresolved"
            );

            // With 32 writes parked on readiness, the blocking pool must still
            // be usable by anything else in the process.
            let probe = tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking(|| 42_u8),
            )
            .await
            .expect("readiness waiters starved the Tokio blocking pool")
            .expect("probe task panicked");
            assert_eq!(probe, 42);
        });
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

    /// H4 acceptance: with `IRONMEM_HARNESS=codex` set in the process env, a
    /// `DaemonConnection`-mode connection whose `initialize` clientInfo says
    /// "claude" must record harness "claude" — the env override must be
    /// ignored for daemon connections, or every connection sharing a daemon
    /// would be force-attributed to whatever harness happened to spawn it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_connection_ignores_env_harness_override() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::set_var("IRONMEM_HARNESS", "codex");
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (mut client_in, server_in) = tokio::io::duplex(4096);
        let (server_out, mut client_out) = tokio::io::duplex(4096);
        client_in
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"claude-code\",\"version\":\"1.0.0\"}}}\n",
            )
            .await
            .unwrap();
        client_in.shutdown().await.unwrap();
        run_server_io_daemon_connection(Arc::clone(&app), BufReader::new(server_in), server_out)
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
            mcp[0].harness, "claude",
            "daemon-connection mode must ignore IRONMEM_HARNESS and use this \
             connection's own clientInfo"
        );

        std::env::remove_var("IRONMEM_HARNESS");
    }

    /// H4 counterpart: a plain stdio-mode connection (`run_server_io`) with NO
    /// clientInfo still honors the `IRONMEM_HARNESS` env override, exactly as
    /// before — the bare `serve` behavior must stay identical.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn stdio_connection_still_honors_env_harness_override() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::set_var("IRONMEM_HARNESS", "codex");
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
        assert_eq!(
            mcp[0].harness, "codex",
            "stdio mode must still honor IRONMEM_HARNESS when no clientInfo is given"
        );

        std::env::remove_var("IRONMEM_HARNESS");
    }
}
