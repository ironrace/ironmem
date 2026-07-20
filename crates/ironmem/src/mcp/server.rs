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

/// A request being dispatched, paired with its stamped sequence number and
/// the request itself so the framing loop can account metrics and release the
/// ordering barrier when it lands.
///
/// Boxed because the loop pushes from two sites — newly read, and released
/// from the mutation queue — and two `async` blocks are two distinct opaque
/// types that cannot share one `FuturesUnordered`. `LocalBoxFuture`, not
/// `BoxFuture`: `Arc<App>` is `!Send` (see `daemon`'s module doc).
type InFlightRequest<'a> =
    futures_util::future::LocalBoxFuture<'a, (u64, JsonRpcRequest, Option<JsonRpcResponse>)>;

/// A one-shot signal that a mutation may release the per-connection ordering
/// barrier BEFORE its response completes, the moment its claim on
/// `collab_wait_my_turn`'s handoff token commits.
///
/// Consumed by `release` so it can be fired at most once, and stamped with the
/// `seq` of the request it was issued to so the framing loop's
/// `release_barrier` can reject it if it ever arrived for the wrong owner
/// (defense in depth — only the barrier owner is ever handed one, see
/// `run_framing_loop`).
struct BarrierRelease {
    tx: tokio::sync::mpsc::UnboundedSender<u64>,
    seq: u64,
}

impl BarrierRelease {
    /// Send the release signal once. A send failure means the framing loop's
    /// receiver is gone — the connection is shutting down or already past the
    /// point of caring — so it is logged at debug and otherwise ignored: the
    /// normal completion path still releases the barrier when this request's
    /// response lands (fail-closed, see design decision 2).
    fn release(self) {
        let seq = self.seq;
        if self.tx.send(seq).is_err() {
            tracing::debug!(seq, "early-release signal dropped: receiver gone");
        }
    }
}

fn dispatch_in_flight<'a>(
    app: &'a Arc<App>,
    seq: u64,
    request: JsonRpcRequest,
    arrived_at: std::time::Instant,
    barrier: Option<BarrierRelease>,
) -> InFlightRequest<'a> {
    Box::pin(async move {
        let response = dispatch_request_with_barrier(app, &request, arrived_at, barrier).await;
        (seq, request, response)
    })
}

/// Whether this request persists state, and so must hold its place in the
/// per-connection ordering barrier ON ENTRY. Derived from
/// `tools::is_mutating_call` — the same predicate that drives read-only mode
/// gating — so the two cannot disagree about what a "write" is.
///
/// This predicate governs barrier ENTRY only — whether a request must become
/// the barrier owner (or queue behind one). Barrier EXIT is a separate
/// decision, keyed on the owning request's stamped `seq` rather than on this
/// predicate — see `release_barrier`.
///
/// Argument-aware, not name-aware: `collab_recv{auto_ack:true}` acks the
/// messages it returns, so classifying by tool name alone would let it overtake
/// a parked write on the same connection. See `tools::CONDITIONALLY_MUTATING_TOOLS`.
fn is_mutating_request(request: &JsonRpcRequest) -> bool {
    if request.method != "tools/call" {
        return false;
    }
    let Some(name) = request_tool_name(request) else {
        return false;
    };
    let empty = serde_json::json!({});
    let args = request.params.get("arguments").unwrap_or(&empty);
    tools::is_mutating_call(name, args)
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
/// `in_flight`. This bounds the NUMBER of queued requests, not their size: the
/// framing loop queues each parsed request whole and argument validation only
/// happens later, on the far side of the barrier, so a client sending very
/// large bodies still pins proportionally more memory. Bounding the count is
/// what keeps the backlog from growing without limit; a byte-level cap in the
/// read arm would be the fix if body size ever becomes the binding constraint.
/// Exceeding it is answered with an explicit error rather than
/// by stalling the reader — a stall would silently re-create the head-of-line
/// blocking this loop exists to avoid, and would be far harder to diagnose.
///
/// Rejecting on overflow BLOCKS this connection's later writes until the
/// backlog drains (`writes_blocked_message`). Refusing the overflowing write on
/// its own would leave the writes behind it free to run, which is precisely the
/// out-of-order execution the queue exists to prevent — the cap would have
/// quietly become a hole in the ordering guarantee.
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
/// Mutations (`tools::is_mutating_call` — argument-aware, so
/// `collab_recv{auto_ack:true}` counts) are held to their arrival order and run
/// one at a time. Letting them overtake would corrupt state, not just reorder
/// replies: only the three embedder-dependent tools park on the readiness
/// gate, so an unordered `delete_drawer` would execute and commit while the
/// `add_drawer` before it was still parked, and the add would then re-create
/// the row the client asked to delete. Reads are unaffected and still overtake
/// freely, which is the whole point of the pipeline.
///
/// The guarantee is precisely: **no mutation executes after a mutation that was
/// refused on this connection.** A mutation is either executed in arrival order
/// or refused; once one is refused for backlog overflow, later mutations are
/// refused too until the backlog drains, so a refusal can never be stepped over.
/// Reads are never refused by this rule.
///
/// The one thing this does NOT promise: a client that keeps streaming writes
/// while ignoring the error responses may see writes accepted again after the
/// backlog drains, since "drained" is the only point at which the connection can
/// know the client has been told. A client that reads its responses before
/// issuing further writes is fully covered.
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
    // Monotonic per-connection request sequence number. Every dispatched
    // request is stamped with the seq it held at admission time, so the
    // mutation barrier below can identify its owner unambiguously even
    // across a stale or duplicate release (see `mutation_barrier`).
    let mut next_seq: u64 = 0;
    // Ordering barrier: mutations run strictly FIFO, one at a time.
    // Each entry keeps its arrival time so a queued mutation's readiness
    // budget is measured from when the client sent it, not from when the
    // barrier released it.
    let mut queued_mutations: VecDeque<(JsonRpcRequest, std::time::Instant)> = VecDeque::new();
    // `Some(seq)` while a mutation is holding the per-connection ordering
    // barrier, naming the seq of the request that currently owns it. Barrier
    // release is keyed on this seq matching (`release_barrier`), not on a
    // bare boolean, so a stale or duplicate release can never clear a
    // *different* owner's barrier out from under it.
    let mut mutation_barrier: Option<u64> = None;
    // Set when a mutation is rejected for queue overflow, cleared once the
    // backlog has fully drained. While set, every further mutation on this
    // connection is rejected too — see `writes_blocked_message`.
    let mut mutations_blocked = false;
    // Early-release channel: `collab_wait_my_turn`'s `BarrierRelease` signals
    // over this the instant its claim commits, so the next queued mutation
    // can start without waiting for the (up to 60s) poll loop that follows.
    // `early_release_tx` is never dropped for the life of this loop — only
    // clones handed to dispatched barrier owners are — so `recv()` below can
    // never spuriously resolve to `None` from every sender disappearing.
    let (early_release_tx, mut early_release_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();

    loop {
        // Checked at the TOP of the loop, before entering `select!`: this is
        // the sole clean-shutdown path (as opposed to the error-return path
        // taken by the read arm's `Err` branch below). The early-release arm
        // below has no `if` guard (by design — `early_release_tx` is kept
        // alive for the whole loop, so `recv()` must never resolve to
        // `None`), which means once
        // `reader_done` is true and `in_flight` is empty, `select!` still has
        // one branch permanently enabled-but-pending. That makes `select!`
        // block forever rather than reach its `else` arm, so the check cannot
        // live after the `select!` the way it used to — control would never
        // return there. Checking here instead lets the loop exit without ever
        // entering the `select!` that would hang on it.
        if reader_done && in_flight.is_empty() && queued_mutations.is_empty() {
            break;
        }

        // Only the dispatched set is capped here. Queued mutations are bounded
        // separately, so a burst of writes can never starve read admission.
        let may_read = !reader_done && in_flight.len() < MAX_IN_FLIGHT_REQUESTS;

        tokio::select! {
            // Biased toward draining: prefer answering work already accepted
            // over accepting more of it.
            biased;

            Some((seq, request, response)) = in_flight.next() => {
                // This request is no longer in flight regardless of whether it
                // still holds the mutation barrier — those are two separate
                // facts. Whether it held the barrier at all, and whether THIS
                // completion is the one that releases it, is decided below by
                // `release_barrier` matching on `seq`.
                let released = release_barrier(&mut mutation_barrier, seq);

                write_and_account(app, &conn, &mut stdout, &request, response).await?;

                // Only the completion that actually released the barrier may
                // drain the queue — a release that found a mismatched (stale
                // or already-cleared) seq must not pop a queued mutation onto
                // a barrier some other owner still holds.
                if released {
                    start_next_queued_mutation(
                        app, &mut queued_mutations, &mut mutation_barrier, &mut next_seq,
                        &mut in_flight, &early_release_tx,
                    );
                }

                // The backlog is empty and nothing is running, so every mutation
                // that arrived before the rejection has been answered. Only now
                // can a new one start without landing after a skipped
                // predecessor, so this is the single point where the connection
                // may accept writes again.
                if mutations_blocked && mutation_barrier.is_none() && queued_mutations.is_empty() {
                    mutations_blocked = false;
                }
            }

            // A mutation dispatched as barrier owner signals here the instant
            // its claim commits (see `BarrierRelease`, `dispatch_wait_my_turn`),
            // rather than waiting for its full response — which for
            // `collab_wait_my_turn` can be up to 60s away. Ordered after the
            // completion arm (biased) so a request that finished in the same
            // poll turn is accounted for before a same-tick early release is
            // considered; ordered before the read arm so a freed barrier is
            // handed to an already-queued mutation before any new request is
            // admitted.
            Some(seq) = early_release_rx.recv() => {
                let released = release_barrier(&mut mutation_barrier, seq);
                if released {
                    start_next_queued_mutation(
                        app, &mut queued_mutations, &mut mutation_barrier, &mut next_seq,
                        &mut in_flight, &early_release_tx,
                    );
                }
                // Deliberately does NOT touch `mutations_blocked`: early
                // release only frees the barrier for a mutation that was
                // already queued before any overflow occurred. It must not
                // re-admit NEW writes on a connection that has already
                // skipped one — that can only happen once the backlog is
                // fully drained, which is decided in the completion arm above.
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
                // Stamped here, before parsing, so every request's readiness
                // budget starts when it actually arrived.
                let arrived_at = std::time::Instant::now();
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
                    // Checked BEFORE the in-flight test: once a write has been
                    // skipped, no later write may execute regardless of whether
                    // the barrier happens to be free at this instant.
                    if mutations_blocked {
                        reject_mutation(
                            app, &conn, &mut stdout, &request, writes_blocked_message(),
                        ).await?;
                        continue;
                    }
                    if mutation_barrier.is_some() {
                        if queued_mutations.len() >= MAX_QUEUED_MUTATIONS {
                            // Rejecting one write would otherwise punch a hole in
                            // the ordering guarantee: the writes behind it would
                            // still run, so a `delete_drawer` could land without
                            // the `add_drawer` it was meant to follow. Block the
                            // connection's writes until the backlog drains.
                            mutations_blocked = true;
                            reject_mutation(
                                app, &conn, &mut stdout, &request, queue_overflow_message(),
                            ).await?;
                            continue;
                        }
                        queued_mutations.push_back((request, arrived_at));
                        continue;
                    }
                    let seq = next_seq;
                    next_seq += 1;
                    mutation_barrier = Some(seq);
                    let barrier = Some(BarrierRelease { tx: early_release_tx.clone(), seq });
                    in_flight.push(dispatch_in_flight(app, seq, request, arrived_at, barrier));
                    continue;
                }

                let seq = next_seq;
                next_seq += 1;
                in_flight.push(dispatch_in_flight(app, seq, request, arrived_at, None));
            }

            // Unreachable while `early_release_tx` lives for the loop's
            // lifetime (see the termination check at the top of the loop);
            // kept as a defensive backstop.
            else => break,
        }
    }

    Ok(())
}

/// Release the per-connection mutation barrier, but ONLY if `seq` is the
/// request that currently owns it. Returns whether it actually cleared.
///
/// Keyed on identity rather than "is something held" so a stale or duplicate
/// release — a completion for a request that never owned the barrier, or one
/// that already released it — can never clear a *different* owner's barrier
/// out from under it. Callers must drain the queue (`start_next_queued_mutation`)
/// only when this returns `true`; see the completion-branch call site in
/// `run_framing_loop`.
fn release_barrier(mutation_barrier: &mut Option<u64>, seq: u64) -> bool {
    if *mutation_barrier == Some(seq) {
        *mutation_barrier = None;
        true
    } else {
        false
    }
}

/// Pop the next queued mutation (if any), admit it as the new barrier owner,
/// and dispatch it — preserving arrival order.
///
/// Must be called only after a `release_barrier` call that returned `true`
/// for the barrier being drained here: that is the sole guarantee that no
/// other request currently owns it, so admitting a new owner cannot collide
/// with one already running.
fn start_next_queued_mutation<'a>(
    app: &'a Arc<App>,
    queued_mutations: &mut std::collections::VecDeque<(JsonRpcRequest, std::time::Instant)>,
    mutation_barrier: &mut Option<u64>,
    next_seq: &mut u64,
    in_flight: &mut futures_util::stream::FuturesUnordered<InFlightRequest<'a>>,
    early_release_tx: &tokio::sync::mpsc::UnboundedSender<u64>,
) {
    if let Some((next, arrived_at)) = queued_mutations.pop_front() {
        let seq = *next_seq;
        *next_seq += 1;
        *mutation_barrier = Some(seq);
        let barrier = Some(BarrierRelease {
            tx: early_release_tx.clone(),
            seq,
        });
        in_flight.push(dispatch_in_flight(app, seq, next, arrived_at, barrier));
    }
}

/// The write that overflowed the per-connection ordering backlog.
fn queue_overflow_message() -> String {
    format!(
        "too many writes queued on this connection ({MAX_QUEUED_MUTATIONS}); \
         this write was NOT applied, and further writes on this connection are \
         refused until the queued ones finish, so no later write can land \
         without it — read the outstanding responses, then retry"
    )
}

/// A write arriving during the blocked window opened by an overflow.
///
/// Deliberately distinct from `queue_overflow_message`: a client needs to tell
/// "you are the write that overflowed" from "you are collateral", because only
/// the first identifies where its sequence actually broke.
fn writes_blocked_message() -> String {
    "writes are blocked on this connection: an earlier write was rejected for \
     queue overflow, and applying this one would place it after a write that \
     never ran. Writes resume once the queued ones finish"
        .to_string()
}

/// Reject one mutation without executing it, and account the response.
///
/// Both refusal paths go through here so they cannot drift in how they report
/// or account — the difference between them is the message alone.
async fn reject_mutation(
    app: &Arc<App>,
    conn: &ConnectionContext,
    stdout: &mut (impl AsyncWrite + Unpin),
    request: &JsonRpcRequest,
    message: String,
) -> Result<(), MemoryError> {
    let tool_name = request_tool_name(request);
    let resp = tool_error_response(
        request.id.clone(),
        tool_name,
        MemoryError::Validation(message),
    );
    let chars = write_response(stdout, &resp).await?;
    account_response_metrics(
        app,
        conn,
        chars,
        tool_name,
        conn.session_id.as_deref(),
        None,
    );
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
///
/// `#[cfg(test)]`: since the barrier-threading refactor, `dispatch_in_flight`
/// (the framing loop's only production dispatch site) calls
/// `dispatch_request_with_barrier` directly, so this thin wrapper has no
/// remaining non-test caller — it exists solely so the pre-existing direct
/// test callers below keep compiling unchanged.
#[cfg(test)]
async fn dispatch_request(
    app: &Arc<App>,
    request: &JsonRpcRequest,
    arrived_at: std::time::Instant,
) -> Option<JsonRpcResponse> {
    dispatch_request_with_barrier(app, request, arrived_at, None).await
}

/// The barrier-aware form of `dispatch_request`. `dispatch_request` itself
/// stays a thin no-barrier wrapper over this — see its doc comment for why —
/// and only `dispatch_in_flight` (the framing loop's actual dispatch site)
/// ever passes a `Some(BarrierRelease)`, and only for the request it
/// dispatched as the mutation barrier owner.
async fn dispatch_request_with_barrier(
    app: &Arc<App>,
    request: &JsonRpcRequest,
    arrived_at: std::time::Instant,
    barrier: Option<BarrierRelease>,
) -> Option<JsonRpcResponse> {
    let tool_name = request.params.get("name").and_then(|value| value.as_str());

    // The only tool that BLOCKS by design rather than by accident. Driven here
    // so its sleep does not hold the dispatch thread — see
    // `dispatch_wait_my_turn`. The only path `barrier` ever reaches: every
    // other tool below drops it unfired, since only a wait's claim can early
    // -release the ordering barrier.
    if request.method == "tools/call" && tool_name == Some("collab_wait_my_turn") {
        return Some(dispatch_wait_my_turn(app, request, arrived_at, barrier).await);
    }

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
        //
        // The budget runs from when the request ARRIVED, not from when it
        // reached this point. Mutations are serialized by the framing loop's
        // ordering barrier, so a fresh per-request timeout would compound:
        // N queued writes against a gate that never resolves would take
        // N x timeout to all report, and the last client would wait hours for
        // a bound documented as 90 seconds.
        let timeout = app
            .config
            .write_readiness_timeout()
            .saturating_sub(arrived_at.elapsed());
        if let Err(error) = app.memory_ready.wait_for_write_async(timeout).await {
            return Some(tool_error_response(request.id.clone(), tool_name, error));
        }
    }

    tokio::task::block_in_place(|| dispatch(app, request))
}

/// The one place a successful `tools/call` result becomes a JSON-RPC response.
/// Shared by `dispatch` and the `collab_wait_my_turn` long-poll path so the two
/// cannot drift in how they frame a result.
fn tool_success_response(
    id: Option<serde_json::Value>,
    content: &serde_json::Value,
) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(content).unwrap_or_default()
            }]
        }),
    )
}

/// `collab_wait_my_turn` is a LONG POLL — up to 60s.
///
/// `dispatch` is synchronous, and per `daemon`'s module doc a synchronous
/// dispatch "stalls this thread, and with it every connection, for its
/// duration" — a bound accepted only because dispatch is short. Polling inside
/// the handler broke that: one agent waiting its turn froze every other
/// connection on the daemon for up to a minute. It is the same class of stall
/// the readiness wait was moved out of the handlers to fix, so it gets the same
/// treatment.
///
/// The generation claim and each snapshot read stay short `block_in_place`
/// calls; only the SLEEP between them moves out here, where
/// `tokio::time::sleep` yields the thread instead of holding it. The claim runs
/// exactly once — re-running it per poll would try to re-consume a one-time
/// handoff token.
///
/// The deadline runs from when the request ARRIVED, matching the readiness
/// wait, so time spent queued behind the ordering barrier counts against the
/// client's requested timeout rather than extending it.
///
/// `barrier` is `Some` only when this request is dispatched as the
/// per-connection mutation barrier owner (see `run_framing_loop`). The claim
/// in `wait_my_turn_begin` is this tool's entire write — once it returns
/// `Ok`, the mutation this barrier represents has fully committed, and the
/// next queued mutation may start without waiting for the (up to 60s) poll
/// loop below to finish. `barrier` is fired exactly there, before the loop,
/// and nowhere else in this function: on the `Err` path it is simply dropped
/// unfired, so the normal completion path releases the barrier instead
/// (fail-closed — see design decision 2).
async fn dispatch_wait_my_turn(
    app: &Arc<App>,
    request: &JsonRpcRequest,
    arrived_at: std::time::Instant,
    barrier: Option<BarrierRelease>,
) -> JsonRpcResponse {
    let tool_name = request_tool_name(request);
    let args = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let deadline = arrived_at + tools::wait_my_turn_timeout(&args);

    if let Err(error) = tokio::task::block_in_place(|| tools::wait_my_turn_begin(app, &args)) {
        return tool_error_response(request.id.clone(), tool_name, error);
    }

    if let Some(barrier) = barrier {
        barrier.release();
    }

    loop {
        match tokio::task::block_in_place(|| tools::wait_my_turn_poll(app, &args)) {
            Err(error) => return tool_error_response(request.id.clone(), tool_name, error),
            Ok((body, settled)) => {
                if settled || std::time::Instant::now() >= deadline {
                    return tool_success_response(request.id.clone(), &body);
                }
            }
        }
        tokio::time::sleep(tools::WAIT_MY_TURN_POLL_INTERVAL).await;
    }
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
                        Ok(content) => Some(tool_success_response(id, &content)),
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

        let write_wait = dispatch_request(&app, &write, std::time::Instant::now());
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
        let search_response = tokio::time::timeout(
            Duration::from_millis(250),
            dispatch_request(&app, &search, std::time::Instant::now()),
        )
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

        let response = tokio::time::timeout(
            Duration::from_secs(2),
            dispatch_request(&app, &invalid, std::time::Instant::now()),
        )
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

        let response = tokio::time::timeout(
            Duration::from_secs(2),
            dispatch_request(&app, &forbidden, std::time::Instant::now()),
        )
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
                .map(|request| dispatch_request(&app, request, std::time::Instant::now()))
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

    /// Drive `run_framing_loop` over `requests` and collect the first `count`
    /// responses. The loop future is `!Send` and cannot be spawned, so the same
    /// `select!` that reads responses also drives the loop — the established
    /// pattern in this module.
    async fn collect_responses(
        app: &Arc<App>,
        mode: TransportMode,
        requests: &[serde_json::Value],
        count: usize,
        bound: Duration,
    ) -> Vec<serde_json::Value> {
        // Large enough to hold every request before the loop starts reading, so
        // the test never deadlocks on a full pipe.
        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        for request in requests {
            client_in
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }

        let mut loop_fut = Box::pin(run_framing_loop(
            app,
            BufReader::new(server_in),
            server_out,
            mode,
        ));
        let mut responses = BufReader::new(client_out).lines();
        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + bound;

        while collected.len() < count {
            tokio::select! {
                result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                line = responses.next_line() => {
                    let line = line.unwrap().expect("a response");
                    collected.push(serde_json::from_str(&line).unwrap());
                }
                _ = tokio::time::sleep_until(deadline) => panic!(
                    "timed out with {} of {count} responses: {collected:?}",
                    collected.len()
                ),
            }
        }
        collected
    }

    fn error_text_of(response: &serde_json::Value) -> String {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a tool error response, got {response}"));
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        payload["error"].as_str().unwrap_or_default().to_string()
    }

    fn add_drawer_request(id: usize) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {
                "name": "add_drawer",
                "arguments": {"content": format!("overflow {id}"), "wing": "race"}
            }
        })
    }

    /// `collab_recv{auto_ack:true}` acks every message it returns, so it is a
    /// write and must hold its place in the ordering barrier — the same
    /// guarantee `delete_drawer` gets in the test above.
    ///
    /// This has to be asserted at the WIRE level, not against
    /// `tools::is_mutating_call` directly: the framing loop reads the arguments
    /// out of `params["arguments"]`, and a predicate wired to `params` instead
    /// would compile, always evaluate false, and leave the ordering half of the
    /// fix completely inert while every unit test on the predicate still passed.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_holds_an_auto_ack_recv_in_mutation_order() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        let (mut client_in, server_in) = tokio::io::duplex(8192);
        let (server_out, client_out) = tokio::io::duplex(8192);
        for request in [
            add_drawer_request(1),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {
                    "name": "collab_recv",
                    "arguments": {
                        "session_id": "s", "receiver": "claude", "auto_ack": true
                    }
                }
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

        // The recv would fail fast (no such session) if it were dispatched, so
        // silence here means the barrier held it, not that it ran slowly.
        let early = tokio::select! {
            result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
            line = responses.next_line() => Some(line.unwrap().unwrap_or_default()),
            _ = tokio::time::sleep(Duration::from_millis(400)) => None,
        };
        assert!(
            early.is_none(),
            "collab_recv{{auto_ack:true}} acks messages, so it must not overtake a \
             write parked on the readiness gate. Got: {early:?}"
        );
    }

    /// The control that keeps the test above honest. Classifying `collab_recv`
    /// as unconditionally mutating would satisfy that test while breaking the
    /// pipelining this whole design exists for, so a PLAIN recv must still
    /// overtake a parked write.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_lets_a_plain_recv_overtake_a_parked_write() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        let requests = [
            add_drawer_request(1),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {
                    "name": "collab_recv",
                    "arguments": {"session_id": "s", "receiver": "claude"}
                }
            }),
        ];

        let first = tokio::time::timeout(
            Duration::from_secs(5),
            first_response_from_connection(&app, TransportMode::DaemonConnection, &requests),
        )
        .await
        .expect("a plain collab_recv is a read and must not be held by the barrier");

        assert_eq!(
            first["id"],
            json!(2),
            "plain collab_recv must be answered while the write is still parked"
        );
    }

    /// Overflowing the ordering backlog used to punch a hole in the very
    /// guarantee the backlog exists to provide: the overflowing write was
    /// rejected, but writes arriving behind it still ran, so a `delete_drawer`
    /// could land without the `add_drawer` it was meant to follow.
    ///
    /// Both refusals must therefore arrive, and with DIFFERENT messages — a
    /// client can only resynchronize if it can tell the write that broke the
    /// sequence from the ones refused as collateral.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_blocks_further_writes_after_a_queue_overflow() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        // id 1 dispatches and parks on the never-resolved gate; ids 2..=65 fill
        // the backlog exactly; 66 overflows it; 67 arrives in the blocked window.
        let overflow_id = MAX_QUEUED_MUTATIONS + 2;
        let collateral_id = overflow_id + 1;
        let requests: Vec<serde_json::Value> =
            (1..=collateral_id).map(add_drawer_request).collect();

        // Only these two get answered; the gate never resolves, so the other 65
        // are still parked or queued when the loop is dropped. Nothing waits out
        // the readiness timeout, which is what keeps this fast.
        let responses = collect_responses(
            &app,
            TransportMode::DaemonConnection,
            &requests,
            2,
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(
            responses
                .iter()
                .map(|r| r["id"].clone())
                .collect::<Vec<_>>(),
            vec![json!(overflow_id), json!(collateral_id)],
            "exactly the overflowing write and the one behind it must be refused"
        );

        let overflow_error = error_text_of(&responses[0]);
        let collateral_error = error_text_of(&responses[1]);
        assert!(
            overflow_error.contains("too many writes queued"),
            "the overflowing write must say so; got {overflow_error:?}"
        );
        assert!(
            collateral_error.contains("writes are blocked on this connection"),
            "a write arriving after an overflow must be refused as collateral, not \
             executed out of order; got {collateral_error:?}"
        );
        assert_ne!(
            overflow_error, collateral_error,
            "the two refusals must be distinguishable or a client cannot tell where \
             its write sequence actually broke"
        );
    }

    /// Blocking WRITES after an overflow must not block reads: the pipeline's
    /// whole purpose is that a read is never held up by a write, and a
    /// fail-closed rule that swallowed reads too would be a worse bug than the
    /// ordering hole it closes.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_are_still_answered_inside_the_blocked_window() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        let overflow_id = MAX_QUEUED_MUTATIONS + 2;
        let read_id = 9999;
        let mut requests: Vec<serde_json::Value> =
            (1..=overflow_id).map(add_drawer_request).collect();
        requests.push(json!({
            "jsonrpc": "2.0", "id": read_id, "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "anything"}}
        }));

        let responses = collect_responses(
            &app,
            TransportMode::DaemonConnection,
            &requests,
            2,
            Duration::from_secs(10),
        )
        .await;

        assert_eq!(
            responses[1]["id"],
            json!(read_id),
            "the read must still be answered while writes are blocked; got {:?}",
            responses[1]
        );
    }

    /// `collab_wait_my_turn` polls for up to 60s. `dispatch` is synchronous and
    /// `daemon`'s module doc is explicit that a synchronous dispatch "stalls
    /// this thread, and with it every connection, for its duration" — so
    /// sleeping inside the handler froze the whole daemon for the length of the
    /// poll, not just the waiting client.
    ///
    /// Asserted the way it actually bites: a long poll on one connection, and a
    /// trivial `status` read behind it that must still come back promptly. The
    /// wait is given 30s and the assertion 5s, so the read can only arrive in
    /// time if the poll released the thread between snapshots.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_poll_does_not_stall_other_requests() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        // A real session, waited on by the agent whose turn it is NOT, so the
        // poll genuinely keeps polling. A missing session would error on the
        // first snapshot and never exercise the wait at all.
        let session_id = uuid::Uuid::new_v4().to_string();
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    &session_id,
                    "/repo",
                    "main",
                    Some("task"),
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();

        let requests = [
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "collab_wait_my_turn",
                    "arguments": {
                        "session_id": session_id, "agent": "codex",
                        "timeout_secs": 30
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "status", "arguments": {}}
            }),
        ];

        let first = tokio::time::timeout(
            Duration::from_secs(5),
            first_response_from_connection(&app, TransportMode::DaemonConnection, &requests),
        )
        .await
        .expect(
            "a request behind a 30s long poll got no answer within 5s — the poll is \
             holding the dispatch thread, which stalls every connection",
        );

        assert_eq!(
            first["id"],
            json!(2),
            "the read must be answered while the long poll is still waiting"
        );
    }

    /// An `AsyncRead` that serves `content` and then FAILS rather than ending.
    ///
    /// A `duplex` half cannot express this: dropping its peer yields EOF, which
    /// is precisely the case a read error has to be distinguished from.
    struct FailingReader {
        content: Vec<u8>,
        offset: usize,
    }

    impl FailingReader {
        fn new(content: &[u8]) -> Self {
            Self {
                content: content.to_vec(),
                offset: 0,
            }
        }
    }

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.offset >= self.content.len() {
                // Content exhausted: the transport BREAKS instead of closing.
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "simulated mid-stream transport failure",
                )));
            }
            let take = (self.content.len() - self.offset).min(buf.remaining());
            let chunk: Vec<u8> = self.content[self.offset..self.offset + take].to_vec();
            buf.put_slice(&chunk);
            self.offset += take;
            std::task::Poll::Ready(Ok(()))
        }
    }

    const ONE_REQUEST: &[u8] =
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";

    /// A mid-stream read failure is NOT a clean close: whatever the client
    /// already sent but the loop has not parsed is lost, and pipelining means
    /// that can be a whole batch rather than a single request. The loop must
    /// surface it so the daemon logs an error close rather than a normal one.
    ///
    /// The writer is a `Vec<u8>`, which never fails, and that choice is load-
    /// bearing: with a `duplex` half whose peer had been dropped, a `BrokenPipe`
    /// on the WRITE side would also produce `Err(MemoryError::Io(_))`, and this
    /// test would pass even with the read-error arm deleted entirely.
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_surfaces_a_mid_stream_read_error() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        let result = run_framing_loop(
            &app,
            BufReader::new(FailingReader::new(ONE_REQUEST)),
            Vec::<u8>::new(),
            TransportMode::DaemonConnection,
        )
        .await;

        assert!(
            matches!(result, Err(MemoryError::Io(_))),
            "a mid-stream read failure must surface as an Io error so the daemon \
             logs an error close; got {result:?}"
        );
    }

    /// The other half of the distinction above, and the reason neither test is
    /// vacuous: same request, same loop, but the stream ENDS instead of failing,
    /// which must be `Ok`. Without this, a loop that returned `Err` on every
    /// close — including normal client disconnects — would satisfy the test
    /// above while being plainly wrong.
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_treats_a_clean_eof_as_a_normal_close() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        let result = run_framing_loop(
            &app,
            BufReader::new(ONE_REQUEST),
            Vec::<u8>::new(),
            TransportMode::DaemonConnection,
        )
        .await;

        assert!(
            result.is_ok(),
            "a stream that ends cleanly must close normally, not as an error; \
             got {result:?}"
        );
    }
}
