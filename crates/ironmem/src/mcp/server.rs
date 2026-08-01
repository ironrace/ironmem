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

/// `collab_start` tools create their session id in the response rather than
/// carrying one in request arguments. Resolve that response's exact session
/// too, so a concurrent scope cannot make its metrics attribution ambiguous.
fn response_collab_session_id(
    request: &JsonRpcRequest,
    response: &JsonRpcResponse,
) -> Option<String> {
    if request.method != "tools/call" {
        return None;
    }
    let tool_name = request.params.get("name").and_then(|v| v.as_str())?;
    if !tool_name.starts_with("collab_") {
        return None;
    }
    response
        .result
        .as_ref()
        .and_then(|result| result.get("content"))
        .and_then(|content| content.as_array())
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(|text| text.as_str())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .and_then(|body| {
            body.get("session_id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .as_deref()
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

#[allow(clippy::too_many_arguments)]
fn account_response_metrics(
    app: &App,
    conn: &ConnectionContext,
    chars: usize,
    tool_name: Option<&str>,
    session_id: Option<&str>,
    request_collab_session_id: Option<&str>,
    exploration: Option<&crate::metrics::ExplorationContext>,
    compact_delta: Option<(usize, usize)>,
) {
    if !crate::search::tunables::metrics_enabled() {
        return;
    }
    tokio::task::block_in_place(|| {
        let metrics_ctx = crate::metrics::MetricsContext::resolve(app, request_collab_session_id);
        crate::metrics::account_mcp_response(
            &app.db,
            chars as i64,
            &mcp_harness(conn),
            tool_name,
            session_id,
            &metrics_ctx,
            exploration,
            compact_delta,
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
type CompactDelta = Option<(usize, usize)>;
type ResponseWithCompactDelta = (JsonRpcResponse, CompactDelta);
type InFlightRequest<'a> = futures_util::future::LocalBoxFuture<
    'a,
    (u64, JsonRpcRequest, Option<ResponseWithCompactDelta>),
>;

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

/// How many mutations dispatched as barrier owner may hold an early-release
/// reservation at once, on one connection.
///
/// Invariant this protects: reads keep at least
/// `MAX_IN_FLIGHT_REQUESTS - MAX_EARLY_RELEASED_WAITS` admission slots even
/// when every early-released wait sits out its full 60s poll. Without this
/// cap, a pipeline of `collab_wait_my_turn` claims could each release the
/// barrier instantly yet still occupy an `in_flight` slot for its whole poll,
/// filling `MAX_IN_FLIGHT_REQUESTS` and starving read admission for up to the
/// long-poll timeout — see `barrier_release_for`.
const MAX_EARLY_RELEASED_WAITS: usize = MAX_IN_FLIGHT_REQUESTS / 4;

// A zero cap would disable early release entirely and silently — every wait
// would revert to holding the ordering barrier for its full poll, with no test
// failing. Tuning `MAX_IN_FLIGHT_REQUESTS` below 4 must break the build instead.
const _: () = assert!(MAX_EARLY_RELEASED_WAITS > 0);

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
/// scoped collab/task-tag context) and a per-connection
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
    // Seqs of requests currently holding an early-release reservation — i.e.
    // dispatched with `Some(BarrierRelease)` and not yet completed. Reserved
    // at DISPATCH time (`barrier_release_for`), not when the signal actually
    // fires, so a request that never calls `BarrierRelease::release` cannot
    // leak a slot: it is freed unconditionally in the completion arm below.
    let mut early_release_reserved: std::collections::HashSet<u64> =
        std::collections::HashSet::new();

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
                // Unconditional and independent of `released`: this seq no
                // longer occupies an in-flight slot regardless of whether it
                // ever held the barrier at all (a plain read, or a mutation
                // dispatched over the cap with `None`, was never reserved —
                // `HashSet::remove` on an absent key is a harmless no-op).
                early_release_reserved.remove(&seq);

                write_and_account(app, &conn, &mut stdout, &request, response).await?;

                // Only the completion that actually released the barrier may
                // drain the queue — a release that found a mismatched (stale
                // or already-cleared) seq must not pop a queued mutation onto
                // a barrier some other owner still holds.
                if released {
                    start_next_queued_mutation(
                        app, &mut queued_mutations, &mut mutation_barrier, &mut next_seq,
                        &mut in_flight, &early_release_tx, &mut early_release_reserved,
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
                        &mut in_flight, &early_release_tx, &mut early_release_reserved,
                    );
                } else if early_release_reserved.contains(&seq) {
                    // Unlike the completion arm — where a non-owning seq is
                    // routine (every read, and any mutation dispatched over the
                    // cap) — only the barrier OWNER is ever handed a
                    // `BarrierRelease`, so a signal for a seq that still holds
                    // its reservation yet no longer owns the barrier is a
                    // should-never-happen: the seq bookkeeping has drifted.
                    //
                    // The one BENIGN stale signal — a wait that settled on its
                    // first poll, so the biased `select!` took its completion
                    // arm first — is excluded by this guard, because that arm
                    // drops the reservation as it runs. Warning on it would
                    // fire on every immediately-settling wait.
                    tracing::warn!(
                        seq,
                        barrier = ?mutation_barrier,
                        "early-release signal for a seq that does not own the mutation barrier \
                         was ignored"
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
                            app,
                            &conn,
                            chars,
                            None,
                            None,
                            conn.session_id.as_deref(),
                            None,
                            None,
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
                        app,
                        &conn,
                        chars,
                        None,
                        None,
                        conn.session_id.as_deref(),
                        None,
                        None,
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
                    let barrier =
                        barrier_release_for(&early_release_tx, &mut early_release_reserved, seq);
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
    early_release_reserved: &mut std::collections::HashSet<u64>,
) {
    if let Some((next, arrived_at)) = queued_mutations.pop_front() {
        let seq = *next_seq;
        *next_seq += 1;
        *mutation_barrier = Some(seq);
        let barrier = barrier_release_for(early_release_tx, early_release_reserved, seq);
        in_flight.push(dispatch_in_flight(app, seq, next, arrived_at, barrier));
    }
}

/// Decide whether the mutation about to be dispatched as barrier owner gets
/// an early-release signal, or must hold the barrier for its full duration
/// like pre-#199 behavior (see `MAX_EARLY_RELEASED_WAITS`).
///
/// Reservation is taken HERE, at dispatch time — not when the signal actually
/// fires — so a request that never calls `BarrierRelease::release` (anything
/// other than a successful `collab_wait_my_turn` claim) cannot leak a
/// reservation slot: it is freed unconditionally in the completion arm of
/// `run_framing_loop`, regardless of whether it was ever used.
fn barrier_release_for(
    early_release_tx: &tokio::sync::mpsc::UnboundedSender<u64>,
    early_release_reserved: &mut std::collections::HashSet<u64>,
    seq: u64,
) -> Option<BarrierRelease> {
    if early_release_reserved.len() >= MAX_EARLY_RELEASED_WAITS {
        // Silent here would read to an operator as "writes are sometimes slow"
        // with no signal at all: this mutation now holds the ordering barrier
        // for its FULL poll (up to the long-poll timeout), stalling every
        // mutation queued behind it on this connection.
        tracing::warn!(
            seq,
            reserved = early_release_reserved.len(),
            cap = MAX_EARLY_RELEASED_WAITS,
            "early-release reservations are at the cap; this mutation will hold the \
             per-connection ordering barrier for its full poll, stalling writes queued behind it"
        );
        return None;
    }
    early_release_reserved.insert(seq);
    Some(BarrierRelease {
        tx: early_release_tx.clone(),
        seq,
    })
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
    let sid = conn
        .session_id
        .clone()
        .or_else(|| request_collab_session_id(request));
    let request_collab_id = request_collab_session_id(request);
    account_response_metrics(
        app,
        conn,
        chars,
        tool_name,
        sid.as_deref(),
        request_collab_id.as_deref(),
        None,
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
    response: Option<ResponseWithCompactDelta>,
) -> Result<(), MemoryError> {
    let Some((resp, compact_delta)) = response else {
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
    let request_collab_id =
        request_collab_session_id(request).or_else(|| response_collab_session_id(request, &resp));
    account_response_metrics(
        app,
        conn,
        chars,
        request_tool_name(request),
        sid.as_deref(),
        request_collab_id.as_deref(),
        exploration.as_ref(),
        compact_delta,
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
    dispatch_request_with_barrier(app, request, arrived_at, None)
        .await
        .map(|(response, _)| response)
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
) -> Option<ResponseWithCompactDelta> {
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
                return Some((
                    tool_error_response(request.id.clone(), tool_name, error),
                    None,
                ));
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
            return Some((
                tool_error_response(request.id.clone(), tool_name, error),
                None,
            ));
        }
    }

    tokio::task::block_in_place(|| dispatch_with_compact_delta(app, request))
}

/// The one place a successful `tools/call` result becomes a JSON-RPC response.
/// Shared by `dispatch` and the `collab_wait_my_turn` long-poll path so the two
/// cannot drift in how they frame a result.
///
/// `tool_name` drives opt-in response compaction (`compact::should_compact`):
/// compaction defaults to OFF, so callers that pass `None` — or any tool not
/// in `compact::COMPACTABLE_TOOLS` — get byte-for-byte the same response
/// shape as before this existed.
fn tool_success_response(
    id: Option<serde_json::Value>,
    content: &serde_json::Value,
    tool_name: Option<&str>,
) -> ResponseWithCompactDelta {
    let original = JsonRpcResponse::success(
        id.clone(),
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(content).unwrap_or_default()
            }]
        }),
    );
    if !super::compact::should_compact(tool_name) {
        return (original, None);
    }

    let compacted_content = super::compact::compact_search_response(content);
    if compacted_content == *content {
        return (original, None);
    }
    let compacted = JsonRpcResponse::success(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&compacted_content).unwrap_or_default()
            }]
        }),
    );
    let original_bytes = serde_json::to_vec(&original)
        .map(|json| json.len())
        .unwrap_or(0);
    let compacted_bytes = serde_json::to_vec(&compacted)
        .map(|json| json.len())
        .unwrap_or(0);
    if compacted_bytes >= original_bytes {
        return (original, None);
    }

    (compacted, Some((original_bytes, compacted_bytes)))
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
/// The deadline is computed by `wait_my_turn_deadline` (`collab_session.rs`),
/// the single named helper holding the deadline formula (design decision 7,
/// `docs/iron/plans/2026-07-19-wait-my-turn-barrier-early-release.md`);
/// the synchronous fallback documents at its own deadline site why plain
/// `now() + timeout` is the degenerate case rather than calling it. For a
/// promptly-dispatched request it still runs from when the request
/// ARRIVED, matching the readiness wait, so time spent queued behind the
/// ordering barrier counts against the client's requested timeout rather than
/// extending it. But a request that queued long enough to nearly exhaust that
/// timeout before its claim committed instead gets a FLOOR measured from the
/// commit instant (`begin_completed_at`) — guaranteeing at least a minimal
/// polling window — capped so it can never stretch the wait past what the
/// client itself asked for from that point.
///
/// `barrier` is `Some` only when this request is dispatched as the
/// per-connection mutation barrier owner (see `run_framing_loop`). The claim
/// in `wait_my_turn_begin` — the generation settle plus the scoped
/// attribution stamp it commits — is this tool's entire write; the poll loop
/// below is read-only. Once `wait_my_turn_begin` returns `Ok`, the mutation
/// this barrier represents has fully committed, and the
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
) -> ResponseWithCompactDelta {
    let tool_name = request_tool_name(request);
    let args = request
        .params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let baseline = match tokio::task::block_in_place(|| tools::wait_my_turn_begin(app, &args)) {
        Ok(baseline) => baseline,
        Err(error) => {
            return (
                tool_error_response(request.id.clone(), tool_name, error),
                None,
            )
        }
    };
    let claim_committed_at = tools::ClaimCommittedAt(std::time::Instant::now());
    let deadline =
        tools::wait_my_turn_deadline(tools::ArrivedAt(arrived_at), claim_committed_at, &args);

    if let Some(barrier) = barrier {
        barrier.release();
    }

    loop {
        match tokio::task::block_in_place(|| tools::wait_my_turn_poll(app, &args, &baseline)) {
            Err(error) => {
                return (
                    tool_error_response(request.id.clone(), tool_name, error),
                    None,
                )
            }
            Ok((body, settled)) => {
                if settled {
                    return tool_success_response(
                        request.id.clone(),
                        &body,
                        Some("collab_wait_my_turn"),
                    );
                }
                if std::time::Instant::now() >= deadline {
                    return tool_success_response(
                        request.id.clone(),
                        &serde_json::json!({ "unchanged": true }),
                        Some("collab_wait_my_turn"),
                    );
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
    dispatch_with_compact_delta(app, request).map(|(response, _)| response)
}

fn dispatch_with_compact_delta(
    app: &App,
    request: &JsonRpcRequest,
) -> Option<ResponseWithCompactDelta> {
    let id = request.id.clone();

    match request.method.as_str() {
        // Metrics attribution (harness/session id) is learned per-connection
        // in `run_framing_loop`'s `ConnectionContext`, not here — `dispatch`
        // has no notion of "which connection" once a single `App` is shared
        // across many (see `ConnectionContext` doc comment). `dispatch` stays
        // a pure request -> response function so its many direct test callers
        // (outside this module) are unaffected by this change.
        "initialize" => Some((
            JsonRpcResponse::success(id, protocol::capabilities_response()),
            None,
        )),

        "tools/list" => {
            let tool_list = tools::tool_definitions(app);
            Some((
                JsonRpcResponse::success(id, serde_json::json!({ "tools": tool_list })),
                None,
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
                        Ok(content) => Some(tool_success_response(id, &content, Some(name))),
                        Err(error) => Some((tool_error_response(id, Some(name), error), None)),
                    }
                }
                None => Some((
                    JsonRpcResponse::error(id, -32602, "Missing tool name"),
                    None,
                )),
            }
        }

        "notifications/initialized" | "notifications/cancelled" => None, // No response

        _ => Some((
            JsonRpcResponse::error(id, -32601, &format!("Unknown method: {}", request.method)),
            None,
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

    /// Parse a successful `tool_success_response`'s `content[0].text` back into
    /// the JSON body `wait_my_turn_poll` produced (`is_my_turn`, `phase`,
    /// `current_owner`, `session_ended`) — the Task 5 tests below assert on
    /// these fields directly rather than on the full JSON-RPC envelope.
    fn wait_response_body(response: &JsonRpcResponse) -> serde_json::Value {
        let result = response
            .result
            .as_ref()
            .expect("tool result must be present");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("content[0].text must be a string");
        serde_json::from_str(text).expect("tool response text must be valid JSON")
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

    #[test]
    fn response_compaction_telemetry_requires_actual_savings() {
        let _env = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
        let content = json!({
            "results": [{"id": "only-one"}],
        });

        let (_, compact_delta) = tool_success_response(Some(json!(1)), &content, Some("search"));

        assert_eq!(compact_delta, None);
    }

    #[test]
    fn response_compaction_telemetry_matches_compact_wire_response() {
        let _env = crate::config::EnvGuard::set("IRONMEM_COMPACT_RESPONSES", "1");
        let content = json!({
            "results": [
                {"id": "a", "score": 1.0, "label": "first"},
                {"id": "b", "score": 2.0, "label": "second"},
                {"id": "c", "score": 3.0, "label": "third"},
            ],
        });

        let (response, compact_delta) =
            tool_success_response(Some(json!(1)), &content, Some("search"));
        let (original_bytes, compacted_bytes) = compact_delta.expect("response must compact");

        assert!(compacted_bytes < original_bytes);
        assert_eq!(
            compacted_bytes,
            serde_json::to_vec(&response)
                .expect("JSON-RPC response must serialize")
                .len()
        );
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

    /// Task 7: `reject_mutation` must mirror `write_and_account`'s attribution
    /// fallback. A connection that never learned a session id (no
    /// `initialize` was ever sent) rejects a collab mutation for queue
    /// overflow; the `mcp_response` row it records must still fall back to
    /// the sanitized collab session id carried in the request's own
    /// arguments — otherwise a headless daemon connection loses collab
    /// attribution the moment one of its writes is refused.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reject_mutation_falls_back_to_request_collab_session_id() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        let _env = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        // id 1 dispatches and parks on the never-resolved gate; ids 2..=65
        // fill the backlog exactly; the overflow id is a collab mutation
        // carrying a `session_id` argument this connection never learned any
        // other way.
        let overflow_id = MAX_QUEUED_MUTATIONS + 2;
        let mut requests: Vec<serde_json::Value> =
            (1..overflow_id).map(add_drawer_request).collect();
        requests.push(json!({
            "jsonrpc": "2.0", "id": overflow_id, "method": "tools/call",
            "params": {
                "name": "collab_send",
                "arguments": {
                    "session_id": "fallback-collab-sess",
                    "sender": "claude",
                    "receiver": "codex",
                    "message": "hi"
                }
            }
        }));

        let responses = collect_responses(
            &app,
            TransportMode::DaemonConnection,
            &requests,
            1,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(responses[0]["id"], json!(overflow_id));
        assert!(
            error_text_of(&responses[0]).contains("too many writes queued"),
            "expected the overflow refusal; got {:?}",
            responses[0]
        );

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let rejected = rows
            .iter()
            .find(|r| r.source == "mcp_response" && r.tool_name.as_deref() == Some("collab_send"))
            .expect("rejected collab_send must still record an mcp_response row");
        assert_eq!(
            rejected.session_id.as_deref(),
            Some("fallback-collab-sess"),
            "with no connection-learned session id, the rejected mutation's row \
             must fall back to the sanitized collab session id from the request"
        );
    }

    /// Control case for the fallback above: when the connection HAS learned a
    /// session id (via `initialize`), that connection-level attribution must
    /// still win over whatever `session_id` argument the rejected request
    /// happens to carry — the fallback is last-resort only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread")]
    async fn reject_mutation_prefers_connection_session_id_over_request_fallback() {
        let _g = METRICS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_METRICS");
        std::env::remove_var("IRONMEM_HARNESS");
        let _env = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let _readiness = force_warming_up(&mut app);

        let overflow_id = MAX_QUEUED_MUTATIONS + 2;
        let mut requests = vec![json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"sessionId": "conn-learned-id"}
        })];
        requests.extend((1..overflow_id).map(add_drawer_request));
        requests.push(json!({
            "jsonrpc": "2.0", "id": overflow_id, "method": "tools/call",
            "params": {
                "name": "collab_send",
                "arguments": {
                    "session_id": "should-be-ignored",
                    "sender": "claude",
                    "receiver": "codex",
                    "message": "hi"
                }
            }
        }));

        let responses = collect_responses(
            &app,
            TransportMode::DaemonConnection,
            &requests,
            2,
            Duration::from_secs(10),
        )
        .await;
        let overflow_response = responses
            .iter()
            .find(|r| r["id"] == json!(overflow_id))
            .expect("overflow response must arrive");
        assert!(
            error_text_of(overflow_response).contains("too many writes queued"),
            "expected the overflow refusal; got {overflow_response:?}"
        );

        let rows = app
            .db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        let rejected = rows
            .iter()
            .find(|r| r.source == "mcp_response" && r.tool_name.as_deref() == Some("collab_send"))
            .expect("rejected collab_send must still record an mcp_response row");
        assert_eq!(
            rejected.session_id.as_deref(),
            Some("conn-learned-id"),
            "connection-learned session id must win over the request's own \
             collab session id argument"
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

    /// An already-settled `collab_wait_my_turn` must send its own successful
    /// response immediately through the production framing loop. The nearby
    /// long-poll test only proves that a different request can overtake an
    /// unsettled wait; it would remain green if this path ignored `settled`
    /// and waited until its full timeout.
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_returns_an_already_settled_wait_immediately() {
        for mode in [TransportMode::Stdio, TransportMode::DaemonConnection] {
            #[allow(clippy::arc_with_non_send_sync)]
            let app = Arc::new(App::open_for_test().unwrap());
            let session_id = uuid::Uuid::new_v4().to_string();
            app.db
                .with_transaction(|tx| {
                    crate::collab::queue::create_session(
                        tx,
                        &session_id,
                        "/repo",
                        "main",
                        Some("task"),
                        crate::collab::Agent::Codex,
                        crate::collab::Agent::Claude,
                    )?;
                    crate::collab::queue::set_implementer(
                        tx,
                        &session_id,
                        crate::collab::Agent::Codex,
                        Some(crate::collab::Agent::Codex),
                    )
                })
                .unwrap();

            let request = json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "collab_wait_my_turn",
                    "arguments": {
                        "session_id": session_id, "agent": "codex", "timeout_secs": 30
                    }
                }
            });
            let responses =
                collect_responses(&app, mode, &[request], 1, Duration::from_millis(500)).await;

            assert_eq!(responses[0]["id"], json!(1));
            let body: serde_json::Value = serde_json::from_str(
                responses[0]["result"]["content"][0]["text"]
                    .as_str()
                    .expect("successful wait response must contain JSON text"),
            )
            .expect("wait response text must be valid JSON");
            assert_eq!(
                body,
                json!({
                    "is_my_turn": true,
                    "phase": "PlanParallelDrafts",
                    "current_owner": "codex",
                    "session_ended": false,
                }),
                "got {body:?}"
            );
        }
    }

    /// An other-owned `collab_wait_my_turn` that remains unsettled through its
    /// timeout returns only the compact unchanged frame through the production
    /// framing loop; settled waits above continue to return their full status.
    #[tokio::test(flavor = "multi_thread")]
    async fn framing_loop_compacts_an_unsettled_wait_timeout() {
        for mode in [TransportMode::Stdio, TransportMode::DaemonConnection] {
            #[allow(clippy::arc_with_non_send_sync)]
            let app = Arc::new(App::open_for_test().unwrap());
            let (session_id, token) = wait_session_and_token(&app);
            let request = wait_request(1, &session_id, &token, 1);

            let responses =
                collect_responses(&app, mode, &[request], 1, Duration::from_secs(2)).await;

            assert_eq!(responses[0]["id"], json!(1));
            let body: serde_json::Value = serde_json::from_str(
                responses[0]["result"]["content"][0]["text"]
                    .as_str()
                    .expect("successful wait response must contain JSON text"),
            )
            .expect("wait response text must be valid JSON");
            assert_eq!(body, json!({"unchanged": true}), "got {body:?}");
        }
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

    // ── Task 3: bounded admission for early-released waits ─────────────────

    /// Build a `collab_wait_my_turn` request that carries a real `handoff_token`
    /// — i.e. one the framing loop's `is_mutating_request` classifies as a
    /// MUTATION (see `tools::CONDITIONALLY_MUTATING_TOOLS`'s `collab_wait_my_turn`
    /// entry), so it actually goes through the mutation-ordering barrier and is
    /// eligible for `barrier_release_for`'s early-release reservation — a
    /// `collab_wait_my_turn` call without a token is a plain read and never
    /// touches any of this.
    fn wait_request(
        id: u64,
        session_id: &str,
        token: &str,
        timeout_secs: u64,
    ) -> serde_json::Value {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {
                "name": "collab_wait_my_turn",
                "arguments": {
                    "session_id": session_id, "agent": "codex",
                    "handoff_token": token, "timeout_secs": timeout_secs
                }
            }
        })
    }

    /// A fresh session (implementer `Claude`) with a freshly issued, unclaimed
    /// `handoff_token` for `Agent::Codex`.
    ///
    /// One (session, agent) pair can hold only ONE pending token at a time
    /// (`collab_actor_generations`'s primary key), so getting N independent,
    /// simultaneously-claimable tokens for N concurrent waits requires N
    /// separate sessions — there is no way to mint several live tokens for one
    /// session.
    ///
    /// Since `agent` here is always `Codex` and every session's `current_owner`
    /// defaults to its `Claude` implementer, a wait claiming this token as
    /// `codex` is never "my turn" and the session is never ended by this
    /// helper, so the resulting wait genuinely long-polls rather than settling.
    fn wait_session_and_token(app: &Arc<App>) -> (String, String) {
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
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();
        let token = app
            .db
            .with_transaction(|tx| {
                crate::collab::handoff::issue_or_reuse_handoff(
                    tx,
                    &session_id,
                    crate::collab::Agent::Codex,
                )
            })
            .unwrap()
            .token;
        (session_id, token)
    }

    /// Give `loop_fut` up to `budget` of cooperative wall-clock time to react to
    /// whatever was just written to its input side, then return.
    ///
    /// Every step Task 3's dispatch path takes synchronously — reading the
    /// freshly-written line, admitting the mutation, `wait_my_turn_begin`'s
    /// claim (which is what stamps the scoped "active collab session"
    /// slot), sending the early-release signal, and the wait's own first,
    /// read-only poll — completes without ever yielding to the
    /// executor, so `budget` only has to be long enough for the runtime to give
    /// `loop_fut` a few turns; it is not a real long-poll wait. A regression that
    /// makes the loop hang instead panics via the `loop_fut` branch rather than
    /// silently reporting "done" with nothing having happened.
    async fn drive_briefly(
        loop_fut: &mut (impl std::future::Future<Output = Result<(), MemoryError>> + Unpin),
        budget: std::time::Duration,
    ) {
        tokio::select! {
            result = &mut *loop_fut => panic!("framing loop exited early: {result:?}"),
            _ = tokio::time::sleep(budget) => {}
        }
    }

    /// Pins the cap-check branch in `barrier_release_for` directly: deleting
    /// that `if early_release_reserved.len() >= MAX_EARLY_RELEASED_WAITS`
    /// check would leave every other test in this module green, since the two
    /// wire-level tests around the cap only exercise it indirectly through a
    /// full framing-loop/duplex harness. This test calls the pure function
    /// itself with no `tokio::test`, no wire harness — just a channel and a
    /// `HashSet` — so it fails immediately, and only, if the cap enforcement
    /// or the "don't insert on refusal" invariant regresses.
    #[test]
    fn barrier_release_for_enforces_the_cap_and_does_not_insert_on_refusal() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let mut reserved: std::collections::HashSet<u64> = std::collections::HashSet::new();

        // Fill the cap exactly: every call up to MAX_EARLY_RELEASED_WAITS gets
        // Some, and the set grows by one each time.
        for seq in 0..MAX_EARLY_RELEASED_WAITS as u64 {
            let result = barrier_release_for(&tx, &mut reserved, seq);
            assert!(result.is_some(), "seq {seq} should be under the cap");
            assert!(reserved.contains(&seq));
        }
        assert_eq!(reserved.len(), MAX_EARLY_RELEASED_WAITS);

        // One more, over the cap: must be refused, and must NOT be inserted —
        // the whole leak-prevention invariant depends on refused seqs never
        // occupying a slot in the set.
        let over_cap_seq = MAX_EARLY_RELEASED_WAITS as u64;
        let refused = barrier_release_for(&tx, &mut reserved, over_cap_seq);
        assert!(
            refused.is_none(),
            "dispatching over the cap must return None"
        );
        assert!(
            !reserved.contains(&over_cap_seq),
            "a refused reservation must not be inserted into the set"
        );
        assert_eq!(
            reserved.len(),
            MAX_EARLY_RELEASED_WAITS,
            "the set must not grow on a refusal"
        );

        // Freeing one slot (simulating the completion-branch removal) makes
        // room for exactly one more admission.
        reserved.remove(&0);
        let admitted_again = barrier_release_for(&tx, &mut reserved, over_cap_seq);
        assert!(
            admitted_again.is_some(),
            "freeing a slot must let the next dispatch get early release again"
        );
    }

    /// Pins `release_barrier`'s seq-match guard directly: deleting it (making
    /// every release unconditionally clear the barrier and report success)
    /// leaves the rest of this module's tests green — see
    /// `a_same_tick_settlement_processes_completion_before_the_stale_release_signal`'s
    /// doc comment, whose own regression claim about this guard does not
    /// actually hold under mutation testing, because nothing happens to be
    /// queued at the moment its stale signal is processed. This test calls the
    /// pure function directly against a barrier already owned by a DIFFERENT
    /// seq, so it fails immediately, and only, if the guard regresses.
    #[test]
    fn release_barrier_ignores_a_stale_or_mismatched_seq() {
        // A different request (seq 7) currently owns the barrier.
        let mut mutation_barrier: Option<u64> = Some(7);

        // A stale/duplicate release for some OTHER seq (5) must be a no-op: it
        // must NOT clear the real owner's barrier, and must report that it did
        // not release anything.
        let released = release_barrier(&mut mutation_barrier, 5);
        assert!(!released, "a mismatched seq must not report a release");
        assert_eq!(
            mutation_barrier,
            Some(7),
            "a mismatched seq must not clear a DIFFERENT owner's barrier"
        );

        // The real owner's OWN release still works normally.
        let released = release_barrier(&mut mutation_barrier, 7);
        assert!(released, "the actual owner's seq must release successfully");
        assert_eq!(mutation_barrier, None);

        // Once cleared, a SECOND release attempt for the same (now-stale) seq
        // must also be a no-op — this is the literal "duplicate release" case
        // (e.g. an early-release signal arriving after the completion arm
        // already released the same seq).
        let released_again = release_barrier(&mut mutation_barrier, 7);
        assert!(
            !released_again,
            "a duplicate release for an already-cleared seq must not report success"
        );
        assert_eq!(mutation_barrier, None);
    }

    /// Task 3: dispatching a batch of token-bearing waits that straddles
    /// `MAX_EARLY_RELEASED_WAITS` must not stall a read pipelined behind them.
    ///
    /// Each of the `MAX_EARLY_RELEASED_WAITS + 8` waits below claims its own
    /// session's handoff token as `codex`, which is never that session's
    /// current owner, and none of the sessions is ever ended — so no wait ever
    /// settles on its own within this test's lifetime. Only the first
    /// `MAX_EARLY_RELEASED_WAITS` get an early-release reservation
    /// (`barrier_release_for`); the next one dispatched is forced to hold the
    /// mutation barrier for its own full poll (fail-closed, pre-#199
    /// behavior), which in turn leaves every wait behind IT stuck in the
    /// per-connection ordering queue, never even dispatched. Every single one
    /// of the 24 therefore remains unanswered for the entire test — which
    /// makes the assertion below unambiguous: if the trailing `search` read's
    /// response arrives at all, it did so while every wait was still
    /// outstanding.
    ///
    /// One MCP process may have one collab session per repository-and-branch
    /// scope "active" for metrics attribution
    /// (`ensure_no_conflicting_process_session`) — a wait claims that scope
    /// for its own session in `wait_my_turn_begin`, once,
    /// and a DIFFERENT session's claim attempt is refused while it owns the
    /// same repository-and-branch scope. Because every helper session uses
    /// that shared test scope, this test clears the bindings between waits;
    /// the cleanup is unrelated to Task 3.
    ///
    /// The clear is RELIABLE only because the claim is confined to
    /// `wait_my_turn_begin`: that runs during the preceding `drive_briefly`,
    /// before the clear, and the poll loop that keeps running afterwards is
    /// write-free. If the claim were re-stamped on every poll (as it was before
    /// the #199 fix), an already-dispatched wait could re-bind the cell between
    /// this clear and the next wait's `wait_my_turn_begin`, refusing that wait
    /// and making this test flaky under CPU load.
    ///
    /// What this test does NOT prove: with `MAX_IN_FLIGHT_REQUESTS = 64` and
    /// only ~17 of the 24 waits ever actually admitted into `in_flight` (the
    /// rest sit in the ordering queue for the whole test, since the one wait
    /// dispatched over the cap holds the barrier for its full poll and blocks
    /// the queue behind it), this is nowhere near exhausting
    /// `MAX_IN_FLIGHT_REQUESTS` itself — that would need close to 64 genuinely
    /// concurrent long-polling waits, which this test does not construct. It
    /// is only an integration-level proof that a batch straddling the
    /// `MAX_EARLY_RELEASED_WAITS` boundary does not stall a read behind it.
    #[tokio::test(flavor = "multi_thread")]
    async fn early_released_waits_are_capped_and_a_read_behind_them_still_answers() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        let wait_count = MAX_EARLY_RELEASED_WAITS + 8;
        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let mut loop_fut = Box::pin(run_framing_loop(
            &app,
            BufReader::new(server_in),
            server_out,
            TransportMode::DaemonConnection,
        ));

        for i in 0..wait_count {
            let (session_id, token) = wait_session_and_token(&app);
            let request = wait_request(i as u64, &session_id, &token, 30);
            client_in
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            drive_briefly(&mut loop_fut, Duration::from_millis(20)).await;
            app.clear_active_collab_session();
        }

        let read_id = 999_999_u64;
        let search = json!({
            "jsonrpc": "2.0", "id": read_id, "method": "tools/call",
            "params": {"name": "search", "arguments": {"query": "anything"}}
        });
        client_in
            .write_all(format!("{search}\n").as_bytes())
            .await
            .unwrap();

        let mut responses = BufReader::new(client_out).lines();
        let first = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                line = responses.next_line() => serde_json::from_str::<serde_json::Value>(
                    &line.unwrap().expect("server must write a response"),
                )
                .expect("response must be valid JSON"),
            }
        })
        .await
        .expect(
            "the read pipelined behind a batch straddling MAX_EARLY_RELEASED_WAITS got no \
             answer within 5s — the connection is stalled, which is exactly the starvation \
             the cap exists to prevent",
        );

        assert_eq!(
            first["id"],
            json!(read_id),
            "the read must be answered while every wait is still outstanding (none of the \
             24 ever settles within this test's window); got {first:?} instead"
        );
    }

    /// Task 3: the early-release reservation taken at dispatch must be freed
    /// on COMPLETION, not merely when the mutation barrier itself is freed —
    /// otherwise the slot leaks forever and the cap permanently ratchets down
    /// after the first `MAX_EARLY_RELEASED_WAITS` early-released waits a
    /// connection ever sees.
    ///
    /// Dispatches exactly `MAX_EARLY_RELEASED_WAITS` waits with the minimum
    /// allowed `timeout_secs` (1, per `wait_my_turn_timeout`'s clamp) and lets
    /// every one of them actually settle (their poll expires), which is the
    /// completion event that should free their reservations
    /// (`early_release_reserved.remove` in the completion arm). It then
    /// dispatches one more wait with a long timeout — which never settles on
    /// its own — followed immediately by a queued mutation (another short
    /// wait). If the freed reservations were NOT reclaimed, the new wait would
    /// find the reservation set still full, be dispatched with `None`, and be
    /// forced to hold the barrier for its own full 30s poll — so the queued
    /// mutation behind it would only be answered after that. Observing the
    /// queued mutation's response arrive promptly (well under the new wait's
    /// timeout, and strictly before the new wait's own response) is the proof
    /// the reservations were correctly freed.
    #[tokio::test(flavor = "multi_thread")]
    async fn early_release_reservation_is_freed_on_completion() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let mut loop_fut = Box::pin(run_framing_loop(
            &app,
            BufReader::new(server_in),
            server_out,
            TransportMode::DaemonConnection,
        ));
        let mut responses = BufReader::new(client_out).lines();

        for i in 0..MAX_EARLY_RELEASED_WAITS {
            let (session_id, token) = wait_session_and_token(&app);
            let request = wait_request(i as u64, &session_id, &token, 1);
            client_in
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            drive_briefly(&mut loop_fut, Duration::from_millis(20)).await;
            app.clear_active_collab_session();
        }

        // Let all `MAX_EARLY_RELEASED_WAITS` waits actually settle (their 1s
        // timeout expires), which is the completion event expected to free
        // their reservations.
        let mut settled_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            while settled_ids.len() < MAX_EARLY_RELEASED_WAITS {
                tokio::select! {
                    result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                    line = responses.next_line() => {
                        let line = line.unwrap().expect("a response");
                        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
                        settled_ids.insert(response["id"].as_u64().unwrap());
                    }
                }
            }
        })
        .await
        .expect(
            "the first MAX_EARLY_RELEASED_WAITS waits must all settle within 10s (their \
             timeout_secs is 1) — a regression here is in the polling loop itself, not the \
             reservation logic this test targets",
        );

        // The 16 waits above kept re-claiming the process-local "active collab
        // session" marker on every poll until they settled (see
        // `wait_session_and_token`'s doc comment) and were never cleaned up
        // afterward, so it is still pinned to whichever of them polled last.
        // Clear it here for the same reason the dispatch loop above does:
        // this next wait uses yet another distinct session, so an unrelated
        // leftover "active" session would otherwise reject its claim outright
        // as a cross-session conflict before it ever reaches the reservation
        // logic this test targets.
        app.clear_active_collab_session();

        // One more wait (long timeout, never settles on its own) plus a
        // mutation queued right behind it.
        let (new_session, new_token) = wait_session_and_token(&app);
        let new_wait_id = 9_000_u64;
        let new_wait = wait_request(new_wait_id, &new_session, &new_token, 30);
        client_in
            .write_all(format!("{new_wait}\n").as_bytes())
            .await
            .unwrap();
        drive_briefly(&mut loop_fut, Duration::from_millis(20)).await;
        app.clear_active_collab_session();

        let (queued_session, queued_token) = wait_session_and_token(&app);
        let queued_id = 9_100_u64;
        let queued = wait_request(queued_id, &queued_session, &queued_token, 1);
        client_in
            .write_all(format!("{queued}\n").as_bytes())
            .await
            .unwrap();

        let queued_response = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                    line = responses.next_line() => {
                        let line = line.unwrap().expect("a response");
                        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
                        assert_ne!(
                            response["id"], json!(new_wait_id),
                            "the NEW wait's own response arrived before the mutation queued \
                             behind it — its reservation must have leaked from the earlier \
                             batch, forcing it to hold the barrier for its full 30s timeout \
                             instead of releasing early"
                        );
                        if response["id"] == json!(queued_id) {
                            break response;
                        }
                    }
                }
            }
        })
        .await
        .expect(
            "the mutation queued behind the new wait got no answer within 5s — if the \
             earlier MAX_EARLY_RELEASED_WAITS completions did not free their reservations, \
             the new wait would hold the barrier for its own 30s timeout instead of \
             releasing early, and this queued mutation would be stuck behind it the whole \
             time",
        );
        assert_eq!(queued_response["id"], json!(queued_id));
    }

    // ── Task 5: minimum-window behavior tests ───────────────────────────────
    //
    // Task 4 added `wait_my_turn_deadline`'s floor (`max(arrived_at + timeout,
    // begin_completed_at + min(timeout, WAIT_MY_TURN_MIN_POLL_WINDOW))`) with
    // only pure unit tests on the helper itself. These four tests are the
    // missing integration-level proof that `dispatch_wait_my_turn`, driven
    // end-to-end, actually behaves correctly under the floor: a request that
    // queued long enough for its own arrival deadline to already be in the
    // past still gets a real poll cycle to observe a turn-flip, an
    // already-settled wait is unaffected by the floor, an invalid request
    // still fails fast, and an ordinary promptly-dispatched wait still honors
    // its own requested bound.

    /// A request whose `arrived_at` is already older than `timeout_secs` (as
    /// if it queued behind other mutations for a long time before this
    /// task's early-release logic could dispatch it) must still get at least
    /// one real poll cycle to observe a turn-flip, instead of the pre-Task-4
    /// behavior of the poll loop's first deadline check already being in the
    /// past and returning "not my turn" before the flip below can land.
    ///
    /// Regression-sensitivity (see the commit message / self-review for this
    /// task): reverting `wait_my_turn_deadline`'s `max(...)` back to plain
    /// `arrived_at + timeout` was verified locally to make this specific test
    /// FAIL — the response comes back with `is_my_turn: false` well before
    /// the concurrent flip below ever runs, because the deadline collapses to
    /// a point already in the past.
    #[tokio::test(flavor = "multi_thread")]
    async fn delayed_wait_still_observes_a_turn_flip_after_one_poll_interval() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (session_id, token) = wait_session_and_token(&app);

        let timeout_secs = 5u64;
        let request = tool_call(
            1,
            "collab_wait_my_turn",
            json!({
                "session_id": session_id, "agent": "codex",
                "handoff_token": token, "timeout_secs": timeout_secs
            }),
        );
        // Simulate a request that sat queued for a long time before dispatch:
        // `arrived_at + timeout_secs` is already in the past.
        let arrived_at = std::time::Instant::now() - Duration::from_secs(timeout_secs + 5);

        // `App` is `!Send` (interior `RefCell`s in its connection pool), so
        // this cannot be a `tokio::spawn`ed task on a different worker thread
        // — it has to be a plain future driven cooperatively on the same
        // task as the wait itself, via `tokio::join!`. That still lands the
        // flip concurrently with the wait's polling: both futures only make
        // progress at `.await` points, and the wait yields to the executor
        // every `WAIT_MY_TURN_POLL_INTERVAL` via `tokio::time::sleep`.
        let flip = async {
            // Roughly one poll interval, so the flip lands between the wait's
            // first and second snapshot reads rather than before dispatch
            // even starts.
            tokio::time::sleep(tools::WAIT_MY_TURN_POLL_INTERVAL - Duration::from_millis(150))
                .await;
            app.db
                .with_transaction(|tx| {
                    crate::collab::queue::set_implementer(
                        tx,
                        &session_id,
                        crate::collab::Agent::Claude,
                        Some(crate::collab::Agent::Codex),
                    )
                })
                .unwrap();
        };
        let wait = dispatch_wait_my_turn(&app, &request, arrived_at, None);

        let (_, (response, _)) =
            tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(flip, wait) })
                .await
                .expect(
                    "dispatch_wait_my_turn did not return within 3s — the floor should have kept \
             it polling long enough to observe the concurrent turn-flip",
                );

        let body = wait_response_body(&response);
        assert_eq!(
            body["is_my_turn"],
            json!(true),
            "the wait must observe the concurrent turn-flip instead of returning \
             immediately with an already-past deadline; got {body:?}"
        );
    }

    /// An already-settled wait (the turn is flipped BEFORE dispatch, not
    /// concurrently) must return at once despite carrying the same kind of
    /// old `arrived_at` as the delayed-wait test above — the floor extends
    /// how long an UNSETTLED wait may poll, it must never delay a settled
    /// answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_wait_already_settled_at_dispatch_returns_immediately_despite_an_old_arrival() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (session_id, token) = wait_session_and_token(&app);

        // Flip the turn to `codex` BEFORE dispatching at all, so the very
        // first snapshot read already settles.
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::set_implementer(
                    tx,
                    &session_id,
                    crate::collab::Agent::Claude,
                    Some(crate::collab::Agent::Codex),
                )
            })
            .unwrap();

        let timeout_secs = 30u64;
        let request = tool_call(
            1,
            "collab_wait_my_turn",
            json!({
                "session_id": session_id, "agent": "codex",
                "handoff_token": token, "timeout_secs": timeout_secs
            }),
        );
        let arrived_at = std::time::Instant::now() - Duration::from_secs(timeout_secs + 5);

        let (response, _) = tokio::time::timeout(
            Duration::from_millis(300),
            dispatch_wait_my_turn(&app, &request, arrived_at, None),
        )
        .await
        .expect(
            "an already-settled wait must return in well under one poll interval — the \
             extended floor deadline must never delay a settled answer",
        );

        let body = wait_response_body(&response);
        assert_eq!(body["is_my_turn"], json!(true), "got {body:?}");
    }

    /// A request that fails validation (`wait_my_turn_begin` returns `Err`
    /// before any deadline is ever computed) must still return immediately —
    /// the floor only applies once a claim has actually committed.
    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_wait_request_returns_immediately_without_polling() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());

        // `session_id` is required and absent here, so `wait_my_turn_begin`
        // fails validation before `wait_my_turn_deadline` is ever called.
        let request = tool_call(
            1,
            "collab_wait_my_turn",
            json!({ "agent": "codex", "timeout_secs": 30 }),
        );
        let arrived_at = std::time::Instant::now() - Duration::from_secs(35);

        let (response, _) = tokio::time::timeout(
            Duration::from_millis(200),
            dispatch_wait_my_turn(&app, &request, arrived_at, None),
        )
        .await
        .expect("an invalid wait request must fail validation immediately, not poll");

        let message = tool_error_text(&response);
        assert!(
            message.contains("session_id"),
            "expected a validation error about the missing session_id; got {message:?}"
        );
    }

    /// Control: an ordinary promptly-dispatched wait (`arrived_at ==
    /// dispatch time`, no simulated queueing delay) whose turn never flips
    /// must still honor its own requested `timeout_secs` — neither
    /// collapsing to near-zero (deadline too short) nor stretching past 1s
    /// (the floor overriding the client's own requested bound, which
    /// design decision 7 explicitly rules out).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_prompt_wait_honors_its_own_requested_timeout() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        // `codex` is never made the owner, so this wait genuinely never
        // settles on its own and always runs out its full deadline.
        let (session_id, token) = wait_session_and_token(&app);

        let timeout_secs = 1u64;
        let request = tool_call(
            1,
            "collab_wait_my_turn",
            json!({
                "session_id": session_id, "agent": "codex",
                "handoff_token": token, "timeout_secs": timeout_secs
            }),
        );
        let arrived_at = std::time::Instant::now();

        let started = std::time::Instant::now();
        let (response, _) = tokio::time::timeout(
            Duration::from_millis(1800),
            dispatch_wait_my_turn(&app, &request, arrived_at, None),
        )
        .await
        .expect(
            "a promptly-dispatched 1s wait must resolve within 1.8s — if the floor \
             stretched it past its own requested timeout, this bound would be exceeded",
        );
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(900),
            "the wait resolved after only {elapsed:?} — the deadline collapsed to \
             something far shorter than the requested 1s timeout"
        );

        let body = wait_response_body(&response);
        assert_eq!(
            body,
            json!({ "unchanged": true }),
            "the turn was never flipped, so the deadline must return the compact unchanged \
             frame; got {body:?}"
        );
    }

    // ── Task 6: early-release ordering + fail-closed regression coverage ───
    //
    // The four tests below are the final proof that Tasks 1-5's early-release
    // mechanism cannot reorder mutations or leak the ordering barrier. Every
    // assertion here is on the ORDER responses are WRITTEN (`response["id"]`
    // as read off the wire), not on wall-clock gaps — timing bounds exist
    // only to keep the tests fast and fail-fast on a real hang.

    /// The wire-level ordering proof, and the primary evidence for the
    /// "signal-before-completion" direction of design decision 5: a real,
    /// token-bearing wait that never settles on its own, with a follow-on
    /// mutation and a third mutation pipelined directly behind it on one
    /// connection.
    ///
    /// The wait's `BarrierRelease` fires (freeing the barrier for the
    /// follow-on, then the third write) the instant its claim commits —
    /// long before its own multi-second poll loop ever finishes. When that
    /// poll loop finally DOES finish and its completion reaches the framing
    /// loop, `release_barrier` finds `mutation_barrier` already `None` (nothing
    /// left queued), so the stale completion cannot pop a fourth response out
    /// of nowhere. Exactly 3 responses are collected below with a bounded
    /// timeout; a regression that made the wait's own late completion
    /// re-trigger `start_next_queued_mutation` or otherwise misbehave would
    /// either produce a 4th id inside the window or simply hang.
    ///
    /// Regression sensitivity: reverting `dispatch_wait_my_turn` to fire
    /// `barrier.release()` only from the normal completion path (deleting the
    /// early call) would force the follow-on and third write to wait out this
    /// wait's full `timeout_secs` before running at all — the ids below would
    /// then arrive in a different order, or the bound would simply time out.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_early_released_wait_lets_the_follow_on_and_third_write_finish_before_it_does() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        // Never settles on its own (`codex` is never made the owner and the
        // session is never ended), so its own response can only arrive by
        // running out its `timeout_secs` — giving the follow-on and third
        // write a wide, unambiguous window to finish first.
        let (session_id, token) = wait_session_and_token(&app);

        let timeout_secs = 2u64;
        let wait_id = 1u64;
        let follow_on_id = 2usize;
        let third_id = 3usize;

        let requests = vec![
            wait_request(wait_id, &session_id, &token, timeout_secs),
            add_drawer_request(follow_on_id),
            add_drawer_request(third_id),
        ];

        let responses = collect_responses(
            &app,
            TransportMode::DaemonConnection,
            &requests,
            3,
            Duration::from_secs(timeout_secs + 5),
        )
        .await;

        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(
            ids,
            vec![json!(follow_on_id), json!(third_id), json!(wait_id)],
            "the follow-on and third write must both be answered — in arrival \
             order — before the wait's own response, which can only land once \
             its {timeout_secs}s timeout actually elapses; got {ids:?}"
        );

        // The wait itself must still report its compact, unsettled timeout
        // outcome — early release frees the BARRIER, not the wait's own
        // answer.
        let wait_text = responses[2]["result"]["content"][0]["text"]
            .as_str()
            .expect("wait response must carry content[0].text");
        let wait_body: serde_json::Value = serde_json::from_str(wait_text).unwrap();
        assert_eq!(
            wait_body,
            json!({ "unchanged": true }),
            "the turn was never flipped, so its deadline must return the compact unchanged \
             frame; got {wait_body:?}"
        );
    }

    /// The fail-closed proof: a token-bearing wait whose claim FAILS —
    /// `wait_my_turn_begin` returns `Err` because the token presented was
    /// never issued for this `(session_id, agent)` pair — must still hold the
    /// mutation barrier until ITS OWN (immediate) completion, and release it
    /// only through the normal completion path. `barrier` is simply dropped,
    /// unfired, on the `Err` early-return in `dispatch_wait_my_turn` — never
    /// released, never signaled through the early-release channel.
    ///
    /// Regression this catches: firing `barrier.release()` unconditionally
    /// (or before the `Ok` check) would let the follow-on mutation start
    /// concurrently with, or even before, this failed wait's own error
    /// response — this test would then see the follow-on's id arrive before,
    /// or racing, the wait's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_claim_holds_the_barrier_until_its_own_error_response() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        // A real session with a real, unclaimed token issued for `codex` —
        // deliberately NOT used below. The request instead carries a
        // freshly-minted UUID that was never issued for this session, so
        // `claim_handoff_token` rejects it (`pending_token != token`).
        let (session_id, _real_token) = wait_session_and_token(&app);
        let bogus_token = uuid::Uuid::new_v4().to_string();

        let wait_id = 1u64;
        let follow_on_id = 2usize;
        let requests = vec![
            wait_request(wait_id, &session_id, &bogus_token, 30),
            add_drawer_request(follow_on_id),
        ];

        let responses = collect_responses(
            &app,
            TransportMode::DaemonConnection,
            &requests,
            2,
            Duration::from_secs(2),
        )
        .await;

        let ids: Vec<_> = responses.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(
            ids,
            vec![json!(wait_id), json!(follow_on_id)],
            "the failed wait's own error response must be written before the \
             follow-on queued behind it — the barrier must have been held \
             until this request's own (immediate) completion; got {ids:?}"
        );

        assert_eq!(
            responses[0]["result"]["isError"].as_bool(),
            Some(true),
            "a failed claim must produce an error response; got {:?}",
            responses[0]
        );
        let message = error_text_of(&responses[0]);
        assert!(
            message.contains("invalid handoff_token"),
            "expected a claim-rejection message; got {message:?}"
        );
    }

    /// The "completion-before-signal" ordering — the trickier of the two
    /// possible race outcomes between a wait's own normal completion and its
    /// early-release channel signal (the companion, "signal-before-completion"
    /// direction is what
    /// `an_early_released_wait_lets_the_follow_on_and_third_write_finish_before_it_does`
    /// proves above, end to end).
    ///
    /// `barrier.release()` fires the instant `wait_my_turn_begin` returns
    /// `Ok`, strictly BEFORE the poll loop runs even once. If the very first
    /// `wait_my_turn_poll` already settles (the turn is flipped to `codex`
    /// BEFORE this wait is ever dispatched), `dispatch_wait_my_turn`'s entire
    /// body — claim, release, one poll, return — executes on a SINGLE poll of
    /// its future, without ever reaching `tokio::time::sleep`. That means
    /// both the completed future (`in_flight`) AND the queued channel message
    /// (`early_release_rx`) become ready inside the very same `select!` pass.
    /// Because the `select!` in `run_framing_loop` is `biased` with the
    /// completion arm listed BEFORE the early-release arm, the completion arm
    /// always wins that race: this wait's barrier release is therefore first
    /// evaluated via NORMAL completion (which succeeds and would drain the
    /// queue if anything were queued), and the early-release channel's
    /// message for the SAME seq is only drained on a LATER loop pass — by
    /// which point `mutation_barrier` no longer matches, so `release_barrier`
    /// must reject it as stale (a no-op), not re-drain the queue or clear
    /// anything a second time.
    ///
    /// Constructed by flipping `current_owner` to `codex` via
    /// `set_implementer` BEFORE the wait is ever dispatched — the same
    /// technique
    /// `a_wait_already_settled_at_dispatch_returns_immediately_despite_an_old_arrival`
    /// (Task 5) uses — so the wait's own first snapshot read already
    /// settles.
    ///
    /// What this test actually pins: the completion-before-signal ORDERING
    /// (the three ids below, in that order) and that the stale signal
    /// produces no observable fourth response. It does NOT, by itself, prove
    /// `release_barrier`'s seq-match guard is load-bearing: mutation testing
    /// shows that deleting that guard entirely leaves this test green too,
    /// because `queued_mutations` is already empty by the time the stale
    /// signal is drained here, so an unconditional clear-and-drain is a no-op
    /// in this specific construction. The guard's regression coverage lives
    /// in the direct unit test `release_barrier_ignores_a_stale_or_mismatched_seq`,
    /// which calls `release_barrier` against a barrier already owned by a
    /// DIFFERENT seq and confirms a mismatched or duplicate release is
    /// rejected.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_same_tick_settlement_processes_completion_before_the_stale_release_signal() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (session_id, token) = wait_session_and_token(&app);

        // Flip the turn to `codex` BEFORE dispatching the wait at all, so its
        // very first snapshot read already settles — no sleep is ever
        // reached.
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::set_implementer(
                    tx,
                    &session_id,
                    crate::collab::Agent::Claude,
                    Some(crate::collab::Agent::Codex),
                )
            })
            .unwrap();

        let wait_id = 1u64;
        let follow_on_id = 2usize;
        let second_follow_on_id = 3usize;

        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let mut loop_fut = Box::pin(run_framing_loop(
            &app,
            BufReader::new(server_in),
            server_out,
            TransportMode::DaemonConnection,
        ));
        let mut responses = BufReader::new(client_out).lines();

        for request in [
            wait_request(wait_id, &session_id, &token, 30),
            add_drawer_request(follow_on_id),
            add_drawer_request(second_follow_on_id),
        ] {
            client_in
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }

        // Collect exactly 3 responses: the already-settled wait, plus the two
        // mutations freed behind it.
        let mut collected = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while collected.len() < 3 {
                tokio::select! {
                    result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                    line = responses.next_line() => {
                        let line = line.unwrap().expect("a response");
                        collected.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
                    }
                }
            }
        })
        .await
        .expect("all three responses (wait + two queued mutations) must arrive within 5s");

        let ids: Vec<_> = collected.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(
            ids,
            vec![
                json!(wait_id),
                json!(follow_on_id),
                json!(second_follow_on_id)
            ],
            "the already-settled wait must complete (releasing its barrier via \
             NORMAL completion) before either mutation behind it, and those two \
             must still run in FIFO order behind it; got {ids:?}"
        );

        // No fourth, spurious response: the early-release channel's now-stale
        // message for this same seq must be silently dropped by
        // `release_barrier`'s seq mismatch, not re-fire a queue drain or
        // duplicate anything observable on the wire.
        let extra = tokio::select! {
            result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
            line = responses.next_line() => Some(line.unwrap()),
            _ = tokio::time::sleep(Duration::from_millis(300)) => None,
        };
        assert!(
            extra.is_none(),
            "a stale early-release signal produced an unexpected extra response: {extra:?}"
        );
    }

    /// Design decision 5, explicit coverage: an early release must NEVER
    /// clear `mutations_blocked` — only the completion arm may, and only once
    /// the WHOLE backlog (queue AND barrier) is empty.
    ///
    /// Construction: `add_drawer` (write-shaped) parks indefinitely on a
    /// force-pending readiness gate as the first barrier owner, exactly like
    /// `framing_loop_blocks_further_writes_after_a_queue_overflow`. A backlog
    /// of `MAX_QUEUED_MUTATIONS` mutations queues behind it — the LAST of
    /// which is a real, token-bearing wait — followed by one more mutation
    /// that overflows the backlog (`mutations_blocked = true`) and a
    /// collateral one behind that (both rejected, exactly as in the PR #198
    /// overflow test this reuses). Resolving the gate then lets the whole
    /// backlog cascade: every plain `add_drawer` in front of the wait
    /// completes and hands the barrier onward, and when the wait itself
    /// finally becomes owner it claims its (real, valid) token and fires its
    /// `BarrierRelease` — an early release occurring, this time, strictly
    /// AFTER `mutations_blocked` was already set. A probe mutation sent right
    /// after must still be rejected as blocked: if a regression ever taught
    /// the early-release arm to also clear `mutations_blocked` (a very
    /// plausible copy-paste of the completion arm's check), this probe would
    /// instead be accepted or queued.
    #[tokio::test(flavor = "multi_thread")]
    async fn mutations_blocked_survives_an_early_release_and_still_gates_new_writes() {
        let _g = EnvGuard::set(WRITE_READINESS_TIMEOUT_ENV, "30");

        #[allow(clippy::arc_with_non_send_sync)]
        let mut app = Arc::new(App::open_for_test().unwrap());
        let readiness = force_warming_up(&mut app);
        let (session_id, token) = wait_session_and_token(&app);

        let q = MAX_QUEUED_MUTATIONS;
        // id 1 dispatches and parks on the never-(yet)-resolved gate; ids
        // 2..=q are plain fillers; id (q+1) — the LAST queued entry — is the
        // real wait; (q+2) overflows the backlog; (q+3) is collateral.
        let special_wait_id = (q + 1) as u64;
        let overflow_id = q + 2;
        let collateral_id = q + 3;

        let (mut client_in, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, client_out) = tokio::io::duplex(1 << 20);
        let mut loop_fut = Box::pin(run_framing_loop(
            &app,
            BufReader::new(server_in),
            server_out,
            TransportMode::DaemonConnection,
        ));
        let mut responses = BufReader::new(client_out).lines();

        client_in
            .write_all(format!("{}\n", add_drawer_request(1)).as_bytes())
            .await
            .unwrap();
        drive_briefly(&mut loop_fut, Duration::from_millis(100)).await;

        for i in 2..=q {
            client_in
                .write_all(format!("{}\n", add_drawer_request(i)).as_bytes())
                .await
                .unwrap();
        }
        let special_wait = wait_request(special_wait_id, &session_id, &token, 5);
        client_in
            .write_all(format!("{special_wait}\n").as_bytes())
            .await
            .unwrap();
        drive_briefly(&mut loop_fut, Duration::from_millis(100)).await;

        client_in
            .write_all(format!("{}\n", add_drawer_request(overflow_id)).as_bytes())
            .await
            .unwrap();
        client_in
            .write_all(format!("{}\n", add_drawer_request(collateral_id)).as_bytes())
            .await
            .unwrap();

        let mut collected = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while collected.len() < 2 {
                tokio::select! {
                    result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                    line = responses.next_line() => {
                        let line = line.unwrap().expect("a response");
                        collected.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
                    }
                }
            }
        })
        .await
        .expect("the overflow and collateral rejections must arrive promptly");

        assert_eq!(
            collected[0]["id"],
            json!(overflow_id),
            "got {:?}",
            collected[0]
        );
        assert_eq!(
            collected[1]["id"],
            json!(collateral_id),
            "got {:?}",
            collected[1]
        );
        assert!(
            error_text_of(&collected[0]).contains("too many writes queued"),
            "got {:?}",
            collected[0]
        );
        assert!(
            error_text_of(&collected[1]).contains("writes are blocked on this connection"),
            "got {:?}",
            collected[1]
        );

        // Resolve readiness: id 1 completes, and the whole backlog cascades
        // — every plain filler hands the barrier onward, and the real wait at
        // the end of the queue claims its token and fires an EARLY release,
        // strictly after `mutations_blocked` was set above.
        readiness.resolve_ready();
        drive_briefly(&mut loop_fut, Duration::from_millis(500)).await;

        // A brand-new mutation sent now must STILL be rejected as blocked:
        // the wait's early release (the early-release `select!` arm)
        // deliberately does not touch `mutations_blocked` — only the
        // completion arm does, and that has not run for the wait itself yet
        // (it is still polling toward its own settlement).
        let probe_id = q + 4;
        client_in
            .write_all(format!("{}\n", add_drawer_request(probe_id)).as_bytes())
            .await
            .unwrap();

        let probe_response = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    result = &mut loop_fut => panic!("framing loop exited early: {result:?}"),
                    line = responses.next_line() => {
                        let line = line.unwrap().expect("a response");
                        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                        if value["id"] == json!(probe_id) {
                            break value;
                        }
                        // Otherwise: one of the cascading backlog responses
                        // draining in the background — not the one we're
                        // waiting for; keep going.
                    }
                }
            }
        })
        .await
        .expect("the probe mutation got no answer within 2s");

        let probe_error = error_text_of(&probe_response);
        assert!(
            probe_error.contains("writes are blocked on this connection"),
            "a new mutation arriving after the wait's early release must still be \
             rejected as blocked — `mutations_blocked` must not have been cleared \
             by the early-release arm; got {probe_error:?}"
        );
    }
}
