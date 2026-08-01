//! Metrics helpers shared by the MCP server (response sizing) and the lifecycle
//! hooks (occupancy sampling), per METRICS_SPEC §5/§6/§8.
//!
//! Two layers live here:
//! - **Pure calc** (`estimate_tokens`, `occupancy_pct`, `extract_last_assistant_usage`,
//!   `hook_event_for`, `now_rfc3339`) — no DB/env access, unit-testable in isolation.
//! - **Best-effort sinks** (`account_mcp_response`, `record_occupancy_sample`) — take a
//!   `&Database` and write metric rows. They never propagate DB errors (logged via
//!   `tracing::warn!`) so a metrics failure cannot break MCP transport or a hook.
//!   `IRONMEM_METRICS`/`IRONMEM_CONTEXT_WINDOW` gating is read fresh by the callers
//!   (`search::tunables`), not here.

pub(crate) mod transcript;

/// Token usage extracted from a transcript's last assistant message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
}

/// METRICS_SPEC §6.2 estimate: `ceil(chars / 4)`.
pub(crate) fn estimate_tokens(chars: i64) -> i64 {
    if chars <= 0 {
        0
    } else {
        // `i64::div_ceil` is unstable; cast through `u64` (guarded > 0 above)
        // for the stable `div_ceil`, avoiding clippy's `manual_div_ceil`.
        (chars as u64).div_ceil(4) as i64
    }
}

/// METRICS_SPEC §8.1: `(input + cache_read) / context_window`.
/// `None` when the window is non-positive (avoids div-by-zero / inversion).
pub(crate) fn occupancy_pct(
    input_tokens: i64,
    cache_read_input_tokens: i64,
    window: i64,
) -> Option<f64> {
    if window <= 0 {
        return None;
    }
    Some((input_tokens + cache_read_input_tokens) as f64 / window as f64)
}

/// RFC3339 UTC timestamp, matching existing metric-row call sites
/// (`chrono::Utc::now().to_rfc3339()`).
pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

use crate::collab::Phase;

/// METRICS_SPEC §3.2: map the session `Phase` to its token_usage bucket.
/// Exhaustive on purpose — a new `Phase` variant must fail compilation here
/// so the spec table gets a conscious update, never a silent `other`.
pub(crate) fn phase_bucket(phase: Phase) -> &'static str {
    match phase {
        Phase::PlanParallelDrafts
        | Phase::PlanSynthesisPending
        | Phase::PlanCopilotReviewPending
        | Phase::PlanFinalizePending
        | Phase::PlanLocked => "planning",
        Phase::CodeImplementPending => "impl",
        Phase::CodeReviewLocalPending | Phase::CodeReviewFinalPending => "review",
        Phase::CodeReviewFixGlobalPending => "rework",
        Phase::CodingComplete | Phase::CodingFailed => "other",
    }
}

/// Attribution context for one token_usage row (METRICS_SPEC §2.3 / §3).
/// Resolved fresh at every row write — phase is read from the session record
/// "at the time the row is recorded" (§3.2), never cached across turns.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MetricsContext {
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub task_tag: Option<String>,
}

impl MetricsContext {
    /// §2.3 priority: an explicit collab id first, otherwise the sole active
    /// scoped binding (both stamp id + phase bucket, INCLUDING
    /// terminal-but-not-ended sessions, which stamp `other`); else the explicit
    /// task tag (phase defaults to `impl` per §3.3).
    ///
    /// An explicit id only DISAMBIGUATES among this process's own bindings — an
    /// id the process holds no binding for is ignored, so inspecting a foreign
    /// session with `collab_status` cannot stamp that session with this
    /// process's tokens.
    ///
    /// Multiple active scopes are intentionally not attributed implicitly, and
    /// that also suppresses the task-tag fallback. Bindings for ended or
    /// missing sessions are pruned before that decision, so a session ended
    /// elsewhere cannot pin the map into ambiguity.
    ///
    /// Ended (`ended_at IS NOT NULL`) or missing sessions clear only their
    /// matching App binding; the discovering row stays unstamped (returns
    /// `MetricsContext::default()`). Best-effort: a DB read error degrades to
    /// an empty context + warn.
    pub(crate) fn resolve(
        app: &crate::mcp::app::App,
        explicit_collab_session_id: Option<&str>,
    ) -> MetricsContext {
        // An explicit id is a disambiguator among THIS process's bindings, not
        // an attribution override. Honouring an unbound id would let a
        // `collab_status` call on another workspace's session stamp that
        // session with this process's tokens. Resolving the scope here also
        // means the self-heal arms below can clear the binding on either path.
        let explicit = explicit_collab_session_id.and_then(|sid| {
            app.scope_of_active_collab_session(sid)
                .map(|scope| (scope, sid.to_string()))
        });
        // One read: branching on the sole binding and on the count separately
        // can observe two different map states.
        let (mut binding_count, mut sole) = app.collab_attribution_snapshot();
        // Only an ambiguous map is worth a sweep, so the common single-binding
        // path pays no extra DB reads.
        if explicit.is_none() && binding_count > 1 {
            prune_stale_collab_bindings(app);
            let refreshed = app.collab_attribution_snapshot();
            binding_count = refreshed.0;
            sole = refreshed.1;
        }
        // Both paths carry the scope the binding was found under, so a
        // missing-session self-heal can always clear exactly that key.
        let scoped = explicit.or(sole);
        if let Some(((repo_path, branch), sid)) = scoped {
            match app.db.collab_load_session_record(&sid) {
                Ok(record) if record.ended_at.is_none() => {
                    return MetricsContext {
                        collab_session_id: Some(sid),
                        collab_phase: Some(phase_bucket(record.session.phase).to_string()),
                        task_tag: None,
                    };
                }
                Ok(record) => {
                    // Session has ended — self-heal only its matching scope.
                    app.clear_active_collab_session_for_scope_if_matches(
                        &sid,
                        &record.repo_path,
                        &record.branch,
                    );
                    return MetricsContext::default();
                }
                // `NotFound` is what the session loader returns for a missing row —
                // matched explicitly so only a confirmed-missing session clears the
                // cell; any new error variant lands in the warn arm below instead of
                // being mistaken for a missing session.
                Err(crate::error::MemoryError::NotFound(_)) => {
                    tracing::warn!(
                        session_id = %sid,
                        "metrics attribution: active collab session not found — clearing its binding"
                    );
                    app.clear_active_collab_session_for_scope_if_matches(&sid, &repo_path, &branch);
                    return MetricsContext::default();
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %sid,
                        error = %e,
                        "metrics: collab session lookup for attribution failed"
                    );
                    return MetricsContext::default();
                }
            }
        }
        // An ambiguous set of scoped sessions is deliberately not allowed to
        // fall through to a task tag: the request may belong to any one of
        // those sessions, and leaving it unstamped is safer than guessing.
        if binding_count == 0 {
            if let Some(tag) = app.explicit_task_tag_snapshot() {
                return MetricsContext {
                    collab_session_id: None,
                    collab_phase: Some("impl".to_string()),
                    task_tag: Some(tag),
                };
            }
        } else {
            // Ambiguity drops both the session id and the task tag, so without
            // this the coverage loss is invisible in the data and in the log.
            tracing::debug!(
                bindings = binding_count,
                "metrics attribution: ambiguous collab scopes — row left unstamped"
            );
        }
        MetricsContext::default()
    }
}

/// Drop attribution bindings whose session has ended or no longer exists.
///
/// `collab_end` and the same-scope guard on a new start are the only other
/// removals, so a session ended from a different process — or a row deleted
/// out from under this one — leaves its scope bound here indefinitely. Two
/// surviving bindings make every unscoped row ambiguous, which suppresses
/// implicit attribution AND the task-tag fallback for as long as the daemon
/// lives, so a single stale entry can silently zero out metrics coverage.
///
/// A session that merely fails to load (transient DB error) is KEPT: dropping
/// it could hand its work to another scope's binding or to a task tag, and
/// misattributed rows are worse than absent ones.
///
/// ponytail: only definitively-dead sessions are reaped — a harness killed
/// mid-session leaves `ended_at IS NULL` and still pins the map until the
/// daemon idles out. Add a last-touched stamp per binding and a TTL sweep if
/// abandoned sessions turn out to be common; that needs care, since evicting a
/// merely-idle live session would resurrect the misattribution this avoids.
fn prune_stale_collab_bindings(app: &crate::mcp::app::App) {
    for (repo_path, branch, session_id) in app.active_collab_sessions_snapshot() {
        let dead = match app.db.collab_load_session_record(&session_id) {
            Ok(record) => record.ended_at.is_some(),
            Err(crate::error::MemoryError::NotFound(_)) => true,
            Err(_) => false,
        };
        if dead {
            tracing::debug!(
                session_id = %session_id,
                repo_path = %repo_path,
                branch = %branch,
                "metrics attribution: pruning binding for ended or missing session"
            );
            app.clear_active_collab_session_for_scope_if_matches(&session_id, &repo_path, &branch);
        }
    }
}

/// Reverse-scan a transcript JSONL string for the LAST assistant message's
/// `usage` object. Mirrors the reverse-scan shape used by the review extractor
/// in `hook.rs`. Handles both a top-level `usage` and a nested
/// `message.usage` envelope. Missing numeric fields default to 0. Returns
/// `None` when no assistant `usage` is found or the input is empty/malformed.
pub(crate) fn extract_last_assistant_usage(raw: &str) -> Option<Usage> {
    for line in raw.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(usage) = find_assistant_usage(&value) {
            return Some(usage);
        }
    }
    None
}

fn find_assistant_usage(value: &serde_json::Value) -> Option<Usage> {
    let is_assistant = value.get("type").and_then(|t| t.as_str()) == Some("assistant")
        || value.get("role").and_then(|t| t.as_str()) == Some("assistant")
        || value
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|t| t.as_str())
            == Some("assistant");
    if !is_assistant {
        return None;
    }
    let usage = value
        .get("usage")
        .or_else(|| value.get("message").and_then(|m| m.get("usage")))?;
    let g = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    Some(Usage {
        input_tokens: g("input_tokens"),
        output_tokens: g("output_tokens"),
        cache_creation_input_tokens: g("cache_creation_input_tokens"),
        cache_read_input_tokens: g("cache_read_input_tokens"),
    })
}

use crate::db::metrics::{MapStatus, NewOccupancySample, NewTokenUsage, SessionSummary};
use crate::db::schema::Database;

/// Exploration-token attribution context for one code-map tool call (Phase 5
/// / issue #94). Populated by `mcp/server.rs` for `code_map_write` /
/// `code_map_load` and passed into `account_mcp_response`. `None` for all
/// other tool calls.
#[derive(Debug, Clone)]
pub(crate) struct ExplorationContext {
    pub turn_id: Option<String>,
    pub area: Option<String>,
    /// `MapStatus::Hit` when the caller found a usable cached map;
    /// `MapStatus::Miss` when no map existed or the tool was `code_map_write`
    /// (write-back).
    pub map_status: Option<MapStatus>,
}

/// Record one MCP response's size (METRICS_SPEC §5.1, Decisions D1/D2/D2b/D6).
/// Always inserts a diagnostic `token_usage` row; atomically accumulates
/// `session_summary.mcp_chars_served` (engine-side, race-free across the
/// MCP-server and hook processes) when a session id is known. Best-effort: all
/// DB errors are logged, never returned.
///
/// When `exploration` is `Some`, the live estimated MCP response row is tagged
/// with Phase-5 code-map attribution. The token proxy remains the response-size
/// estimate (`ceil(chars / 4)`), matching METRICS_SPEC's v0 cost model.
#[allow(clippy::too_many_arguments)]
pub(crate) fn account_mcp_response(
    db: &Database,
    chars: i64,
    harness: &str,
    tool_name: Option<&str>,
    session_id: Option<&str>,
    ctx: &MetricsContext,
    exploration: Option<&ExplorationContext>,
    compact_delta: Option<(usize, usize)>,
) {
    let exploration = exploration.filter(|exp| exp.map_status.is_some());
    let row = NewTokenUsage {
        ts: now_rfc3339(),
        source: "mcp_response".to_string(),
        harness: harness.to_string(),
        model: None,
        tool_name: tool_name.map(|s| s.to_string()),
        session_id: session_id.map(|s| s.to_string()),
        collab_session_id: None,
        collab_phase: None,
        task_tag: None,
        input_tokens: 0,
        output_tokens: estimate_tokens(chars),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        estimated: true,
        chars,
        cost_usd: None,
        map_status: exploration.and_then(|exp| exp.map_status),
        turn_id: exploration.and_then(|exp| exp.turn_id.clone()),
        area: exploration.and_then(|exp| exp.area.clone()),
        original_response_bytes: compact_delta.map(|(original, _)| original as i64),
        compacted_response_bytes: compact_delta.map(|(_, compacted)| compacted as i64),
    }
    .with_context(ctx);
    if let Err(e) = db.insert_token_usage(&row) {
        tracing::warn!("metrics: insert mcp_response token_usage failed: {e}");
    }

    let Some(sid) = session_id else { return };
    // Delta carries ONLY this writer's mcp_chars_served increment; every other
    // column is identity (0 / None) so the atomic upsert leaves hook-owned
    // fields untouched.
    let delta = SessionSummary {
        session_id: sid.to_string(),
        harness: harness.to_string(),
        workspace_root: None,
        started_at: None,
        ended_at: None,
        peak_occupancy_pct: None,
        total_input_tokens: 0,
        total_output_tokens: 0,
        mcp_chars_served: chars,
        compactions: 0,
    };
    if let Err(e) = db.accumulate_session_summary(&delta) {
        tracing::warn!("metrics: accumulate mcp_chars_served failed: {e}");
    }
}

/// Map a hook CLI name to the `occupancy_samples.hook_event` enum value
/// (METRICS_SPEC §5.2 / §8.2). `stop` → `session-stop` (CHECK-constraint safe).
pub(crate) fn hook_event_for(hook_name: &str) -> Option<&'static str> {
    match hook_name {
        "session-start" => Some("session-start"),
        "stop" => Some("session-stop"),
        "precompact" => Some("precompact"),
        "user-prompt-submit" => Some("user-prompt-submit"),
        _ => None,
    }
}

/// Record one occupancy sample + merge the session summary (Decisions D4/D5/D6).
/// Best-effort. Caller guarantees `session_id` is `Some` (absent-id is skipped
/// by the caller per D4). `usage` is `None` when the transcript had no usable
/// assistant usage → a deterministic zero-token sample is still written.
pub(crate) fn record_occupancy_sample(
    db: &Database,
    harness: &str,
    session_id: &str,
    workspace_root: Option<&str>,
    hook_event: &str,
    usage: Option<Usage>,
    window: i64,
) {
    let u = usage.unwrap_or_default();
    let occ = occupancy_pct(u.input_tokens, u.cache_read_input_tokens, window);
    // One clock read for the whole logical event so the sample row and the
    // summary's started_at/ended_at can never drift apart.
    let ts = now_rfc3339();
    let sample = NewOccupancySample {
        ts: ts.clone(),
        harness: harness.to_string(),
        session_id: Some(session_id.to_string()),
        workspace_root: workspace_root.map(|s| s.to_string()),
        hook_event: Some(hook_event.to_string()),
        input_tokens: u.input_tokens,
        cache_read_input_tokens: u.cache_read_input_tokens,
        context_window: window,
        occupancy_pct: occ,
    };
    if let Err(e) = db.insert_occupancy_sample(&sample) {
        tracing::warn!("metrics: insert occupancy_sample failed: {e}");
    }

    // Atomic engine-side merge (preserves mcp_chars_served written by the MCP
    // process; additive fields carry only this event's increment).
    let delta = SessionSummary {
        session_id: session_id.to_string(),
        harness: harness.to_string(),
        workspace_root: workspace_root.map(|s| s.to_string()),
        started_at: Some(ts.clone()),
        ended_at: if hook_event == "session-stop" {
            Some(ts)
        } else {
            None
        },
        peak_occupancy_pct: occ,
        // `user-prompt-submit` fires on EVERY prompt and re-reads the same
        // last-assistant cumulative usage from the transcript tail each time.
        // Accumulating those into the summary token totals would double-count
        // tokens the lifecycle hooks (session-stop/precompact) already add, so
        // zero the per-event delta here (mirroring `mcp_chars_served: 0`). The
        // occupancy_samples row above still carries the real usage, and
        // `peak_occupancy_pct` still updates.
        total_input_tokens: if hook_event == "user-prompt-submit" {
            0
        } else {
            u.input_tokens
        },
        total_output_tokens: if hook_event == "user-prompt-submit" {
            0
        } else {
            u.output_tokens
        },
        mcp_chars_served: 0,
        compactions: if hook_event == "precompact" { 1 } else { 0 },
    };
    if let Err(e) = db.accumulate_session_summary(&delta) {
        tracing::warn!("metrics: accumulate occupancy summary failed: {e}");
    }
}

/// Process-global lock serializing tests that mutate the `IRONMEM_METRICS`
/// env var. Env vars are process-wide, so unrelated test modules that flip the
/// kill switch (here, `search::tunables` and `mcp::server`) must share ONE lock
/// or they clobber each other under the parallel test runner.
#[cfg(test)]
pub(crate) static METRICS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_event_for_maps_user_prompt_submit() {
        assert_eq!(
            hook_event_for("user-prompt-submit"),
            Some("user-prompt-submit")
        );
    }

    #[test]
    fn phase_bucket_maps_every_variant_per_spec_3_2() {
        use crate::collab::Phase;
        // METRICS_SPEC §3.2 table, pinned variant-by-variant.
        assert_eq!(phase_bucket(Phase::PlanParallelDrafts), "planning");
        assert_eq!(phase_bucket(Phase::PlanSynthesisPending), "planning");
        assert_eq!(phase_bucket(Phase::PlanCopilotReviewPending), "planning");
        assert_eq!(phase_bucket(Phase::PlanFinalizePending), "planning");
        assert_eq!(phase_bucket(Phase::PlanLocked), "planning");
        assert_eq!(phase_bucket(Phase::CodeImplementPending), "impl");
        assert_eq!(phase_bucket(Phase::CodeReviewLocalPending), "review");
        assert_eq!(phase_bucket(Phase::CodeReviewFixGlobalPending), "rework");
        assert_eq!(phase_bucket(Phase::CodeReviewFinalPending), "review");
        assert_eq!(phase_bucket(Phase::CodingComplete), "other");
        assert_eq!(phase_bucket(Phase::CodingFailed), "other");
    }

    #[test]
    fn with_context_stamps_collab_fields_and_preserves_rest() {
        let ctx = MetricsContext {
            collab_session_id: Some("collab-1".into()),
            collab_phase: Some("planning".into()),
            task_tag: None,
        };
        let row = crate::db::metrics::NewTokenUsage {
            ts: "2026-06-12T00:00:00Z".into(),
            source: "mcp_response".into(),
            harness: "claude".into(),
            model: None,
            tool_name: None,
            session_id: Some("sess-1".into()),
            collab_session_id: None,
            collab_phase: None,
            task_tag: None,
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: true,
            chars: 8,
            cost_usd: None,
            map_status: None,
            turn_id: None,
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        }
        .with_context(&ctx);
        assert_eq!(row.collab_session_id.as_deref(), Some("collab-1"));
        assert_eq!(row.collab_phase.as_deref(), Some("planning"));
        assert!(row.task_tag.is_none());
        assert_eq!(row.session_id.as_deref(), Some("sess-1")); // untouched
        assert_eq!(row.output_tokens, 2); // untouched
    }

    #[test]
    fn account_mcp_response_tags_live_row_for_code_map_exploration() {
        let db = crate::db::schema::Database::open_in_memory().unwrap();
        let ctx = MetricsContext::default();
        let exploration = ExplorationContext {
            turn_id: Some("turn-1".into()),
            area: Some("core".into()),
            map_status: Some(crate::db::metrics::MapStatus::Hit),
        };

        account_mcp_response(
            &db,
            9,
            "claude",
            Some("code_map_load"),
            None,
            &ctx,
            Some(&exploration),
            None,
        );

        let rows = db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert_eq!(rows.len(), 1, "exploration tags the live row, no duplicate");
        assert!(rows[0].estimated);
        assert_eq!(rows[0].chars, 9);
        assert_eq!(rows[0].tool_name.as_deref(), Some("code_map_load"));
        assert_eq!(rows[0].output_tokens, 3);
        assert_eq!(rows[0].map_status, Some(crate::db::metrics::MapStatus::Hit));
        assert_eq!(rows[0].turn_id.as_deref(), Some("turn-1"));
        assert_eq!(rows[0].area.as_deref(), Some("core"));

        let report = db.report_exploration_delta().unwrap();
        assert_eq!(report.total_turns, 1);
        assert!((report.mean_tokens_map_hit - 3.0).abs() < 1e-9);
    }

    #[test]
    fn account_mcp_response_records_compact_delta() {
        let db = crate::db::schema::Database::open_in_memory().unwrap();
        let ctx = MetricsContext::default();

        account_mcp_response(
            &db,
            100,
            "claude",
            Some("search"),
            None,
            &ctx,
            None,
            Some((200, 100)),
        );

        let rows = db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].original_response_bytes, Some(200));
        assert_eq!(rows[0].compacted_response_bytes, Some(100));
    }

    #[test]
    fn account_mcp_response_null_delta_when_not_compacted() {
        let db = crate::db::schema::Database::open_in_memory().unwrap();
        let ctx = MetricsContext::default();

        account_mcp_response(&db, 100, "claude", Some("search"), None, &ctx, None, None);

        let rows = db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery::default())
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].original_response_bytes, None);
        assert_eq!(rows[0].compacted_response_bytes, None);
    }

    fn test_app() -> std::sync::Arc<crate::mcp::app::App> {
        use crate::config::{Config, EmbedMode, McpAccessMode};
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            db_path: dir.path().join("mem.sqlite3"),
            model_dir: dir.path().join("model"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::Trusted,
            embed_mode: EmbedMode::Noop,
        };
        // Leak the tempdir so the DB file outlives the helper.
        std::mem::forget(dir);
        #[allow(clippy::arc_with_non_send_sync)]
        std::sync::Arc::new(crate::mcp::app::App::new(config).unwrap())
    }

    /// Create a collab session row directly through the queue layer and return its id.
    fn seed_collab_session(app: &crate::mcp::app::App) -> String {
        seed_collab_session_in_scope(app, "ctx-test-session", "/tmp/repo", "main")
    }

    fn seed_collab_session_in_scope(
        app: &crate::mcp::app::App,
        session_id: &str,
        repo_path: &str,
        branch: &str,
    ) -> String {
        let sid = session_id.to_string();
        app.db
            .with_transaction(|tx| {
                crate::collab::queue::create_session(
                    tx,
                    &sid,
                    repo_path,
                    branch,
                    None,
                    crate::collab::Agent::Claude,
                    crate::collab::Agent::Claude,
                )
            })
            .unwrap();
        sid
    }

    #[test]
    fn resolve_stamps_active_collab_session_with_bucket() {
        let app = test_app();
        let sid = seed_collab_session(&app);
        app.set_active_collab_session_for_scope(&sid, "/tmp/repo", "main");
        let ctx = MetricsContext::resolve(&app, None);
        assert_eq!(ctx.collab_session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(ctx.collab_phase.as_deref(), Some("planning")); // new session = PlanParallelDrafts
        assert!(ctx.task_tag.is_none());
    }

    #[test]
    fn resolve_stamps_terminal_but_not_ended_session_as_other() {
        let app = test_app();
        let sid = seed_collab_session(&app);
        // Force the session to a terminal phase WITHOUT ending it.
        app.db
            .with_transaction(|tx| {
                let mut s = crate::collab::queue::load_session(tx, &sid)?;
                s.phase = crate::collab::Phase::CodingComplete;
                crate::collab::queue::save_session(tx, &s)
            })
            .unwrap();
        app.set_active_collab_session_for_scope(&sid, "/tmp/repo", "main");
        let ctx = MetricsContext::resolve(&app, None);
        assert_eq!(ctx.collab_session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(ctx.collab_phase.as_deref(), Some("other"));
    }

    #[test]
    fn resolve_unstamps_and_clears_cell_for_ended_session() {
        let app = test_app();
        let sid = seed_collab_session(&app);
        app.db
            .with_transaction(|tx| crate::collab::queue::end_session(tx, &sid))
            .unwrap();
        app.set_active_collab_session_for_scope(&sid, "/tmp/repo", "main");
        let ctx = MetricsContext::resolve(&app, None);
        assert!(ctx.collab_session_id.is_none());
        assert!(
            app.active_collab_session_snapshot_for_scope("/tmp/repo", "main")
                .is_none(),
            "cell must self-clear"
        );
    }

    #[test]
    fn resolve_does_not_fallback_to_task_tag_for_ended_active_session() {
        let app = test_app();
        let sid = seed_collab_session(&app);
        app.db
            .with_transaction(|tx| crate::collab::queue::end_session(tx, &sid))
            .unwrap();
        app.set_active_collab_session_for_scope(&sid, "/tmp/repo", "main");
        app.set_explicit_task_tag("issue-85");

        let ctx = MetricsContext::resolve(&app, None);

        assert!(ctx.collab_session_id.is_none());
        assert!(ctx.collab_phase.is_none());
        assert!(
            ctx.task_tag.is_none(),
            "the row that discovers a stale collab cell must stay unstamped"
        );
        assert!(app
            .active_collab_session_snapshot_for_scope("/tmp/repo", "main")
            .is_none());
    }

    #[test]
    fn resolve_unstamps_and_clears_cell_for_missing_session() {
        let app = test_app();
        app.set_active_collab_session_for_scope("does-not-exist", "/tmp/repo", "main");
        let ctx = MetricsContext::resolve(&app, None);
        assert!(ctx.collab_session_id.is_none());
        assert!(app
            .active_collab_session_snapshot_for_scope("/tmp/repo", "main")
            .is_none());
    }

    #[test]
    fn resolve_does_not_fallback_to_task_tag_for_missing_active_session() {
        let app = test_app();
        app.set_active_collab_session_for_scope("does-not-exist", "/tmp/repo", "main");
        app.set_explicit_task_tag("issue-85");

        let ctx = MetricsContext::resolve(&app, None);

        assert!(ctx.collab_session_id.is_none());
        assert!(ctx.collab_phase.is_none());
        assert!(
            ctx.task_tag.is_none(),
            "the row that discovers a missing collab cell must stay unstamped"
        );
        assert!(app
            .active_collab_session_snapshot_for_scope("/tmp/repo", "main")
            .is_none());
    }

    #[test]
    fn resolve_falls_back_to_explicit_task_tag_with_impl_default() {
        let app = test_app();
        app.set_explicit_task_tag("issue-85");
        let ctx = MetricsContext::resolve(&app, None);
        assert!(ctx.collab_session_id.is_none());
        assert_eq!(ctx.task_tag.as_deref(), Some("issue-85"));
        assert_eq!(ctx.collab_phase.as_deref(), Some("impl")); // §3.3 default
    }

    #[test]
    fn resolve_collab_session_takes_priority_over_task_tag() {
        let app = test_app();
        let sid = seed_collab_session(&app);
        app.set_active_collab_session_for_scope(&sid, "/tmp/repo", "main");
        app.set_explicit_task_tag("issue-85");
        let ctx = MetricsContext::resolve(&app, None);
        // §2.3 priority: collab id wins; task_tag not stamped alongside it.
        assert_eq!(ctx.collab_session_id.as_deref(), Some(sid.as_str()));
        assert!(ctx.task_tag.is_none());
    }

    #[test]
    fn resolve_ignores_an_explicit_id_not_bound_by_this_process() {
        let app = test_app();
        let local = seed_collab_session_in_scope(&app, "local-session", "/tmp/repo", "main");
        let foreign =
            seed_collab_session_in_scope(&app, "foreign-session", "/tmp/other-repo", "main");
        app.set_active_collab_session_for_scope(&local, "/tmp/repo", "main");

        let ctx = MetricsContext::resolve(&app, Some(&foreign));

        assert_eq!(
            ctx.collab_session_id.as_deref(),
            Some(local.as_str()),
            "a foreign explicit id must not steal this process's metrics attribution"
        );
        assert_ne!(ctx.collab_session_id.as_deref(), Some(foreign.as_str()));
    }

    #[test]
    fn resolve_prunes_an_ended_binding_before_resolving_the_remaining_scope() {
        let app = test_app();
        let live = seed_collab_session_in_scope(&app, "live-session", "/tmp/live", "main");
        let ended = seed_collab_session_in_scope(&app, "ended-session", "/tmp/ended", "main");
        app.db
            .with_transaction(|tx| crate::collab::queue::end_session(tx, &ended))
            .unwrap();
        app.set_active_collab_session_for_scope(&live, "/tmp/live", "main");
        app.set_active_collab_session_for_scope(&ended, "/tmp/ended", "main");

        let ctx = MetricsContext::resolve(&app, None);

        assert_eq!(ctx.collab_session_id.as_deref(), Some(live.as_str()));
        assert!(
            app.active_collab_session_snapshot_for_scope("/tmp/ended", "main")
                .is_none(),
            "an externally ended session must not leave all unscoped work ambiguous"
        );
    }

    #[test]
    fn resolve_prunes_a_missing_binding_before_resolving_the_remaining_scope() {
        let app = test_app();
        let live = seed_collab_session_in_scope(&app, "live-session", "/tmp/live", "main");
        app.set_active_collab_session_for_scope(&live, "/tmp/live", "main");
        app.set_active_collab_session_for_scope("missing-session", "/tmp/missing", "main");

        let ctx = MetricsContext::resolve(&app, None);

        assert_eq!(ctx.collab_session_id.as_deref(), Some(live.as_str()));
        assert!(app
            .active_collab_session_snapshot_for_scope("/tmp/missing", "main")
            .is_none());
    }

    #[test]
    fn resolve_returns_empty_context_when_nothing_set() {
        let app = test_app();
        let ctx = MetricsContext::resolve(&app, None);
        assert!(
            ctx.collab_session_id.is_none() && ctx.collab_phase.is_none() && ctx.task_tag.is_none()
        );
    }

    #[test]
    fn resolve_uses_exact_collab_session_and_leaves_ambiguous_unscoped_request_unstamped() {
        let app = test_app();
        let repo_main = seed_collab_session_in_scope(&app, "ctx-repo-main", "/tmp/repo", "main");
        let other_repo =
            seed_collab_session_in_scope(&app, "ctx-other-repo", "/tmp/other-repo", "main");
        app.set_active_collab_session_for_scope(&repo_main, "/tmp/repo", "main");
        app.set_active_collab_session_for_scope(&other_repo, "/tmp/other-repo", "main");

        let exact = MetricsContext::resolve(&app, Some(other_repo.as_str()));
        assert_eq!(
            exact.collab_session_id.as_deref(),
            Some(other_repo.as_str()),
            "an explicit collab session id must resolve its own scope"
        );
        assert_eq!(exact.collab_phase.as_deref(), Some("planning"));
        assert!(exact.task_tag.is_none());

        let unscoped = MetricsContext::resolve(&app, None);
        assert!(
            unscoped.collab_session_id.is_none()
                && unscoped.collab_phase.is_none()
                && unscoped.task_tag.is_none(),
            "with multiple scoped sessions, an unscoped request must not receive arbitrary attribution"
        );
    }

    #[test]
    fn user_prompt_submit_does_not_accumulate_summary_tokens() {
        // `user-prompt-submit` fires per prompt and re-reads the same cumulative
        // transcript usage each time; without the per-event zeroing the summary
        // token totals would double-count. Two calls with non-zero usage must
        // leave the summary token totals at 0 while still writing sample rows.
        let app = test_app();
        let usage = Some(Usage {
            input_tokens: 1234,
            output_tokens: 56,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 78,
        });
        for _ in 0..2 {
            record_occupancy_sample(
                &app.db,
                "claude",
                "ups-sess",
                Some("/tmp/repo"),
                "user-prompt-submit",
                usage,
                200_000,
            );
        }

        let summary = app
            .db
            .get_session_summary("ups-sess")
            .unwrap()
            .expect("summary row exists");
        assert_eq!(
            summary.total_input_tokens, 0,
            "per-prompt usage must not accumulate into summary input tokens"
        );
        assert_eq!(
            summary.total_output_tokens, 0,
            "per-prompt usage must not accumulate into summary output tokens"
        );

        // The occupancy sample rows themselves are still written with real usage.
        let samples = app
            .db
            .occupancy_samples_for_session("ups-sess", 10)
            .unwrap();
        assert_eq!(samples.len(), 2, "one occupancy sample row per call");
        assert_eq!(samples[0].input_tokens, 1234);
        assert_eq!(samples[0].cache_read_input_tokens, 78);
    }

    #[test]
    fn estimate_tokens_is_ceil_div_4() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(8), 2);
    }

    #[test]
    fn occupancy_pct_uses_input_plus_cache_read_over_window() {
        let pct = occupancy_pct(100_000, 50_000, 200_000).unwrap();
        assert!((pct - 0.75).abs() < 1e-9);
    }

    #[test]
    fn occupancy_pct_none_when_window_nonpositive() {
        assert!(occupancy_pct(1, 1, 0).is_none());
        assert!(occupancy_pct(1, 1, -10).is_none());
    }

    #[test]
    fn extract_last_assistant_usage_reverse_scans_to_last_assistant() {
        let raw = concat!(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":3}}}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{"input_tokens":111,"output_tokens":22,"cache_creation_input_tokens":5,"cache_read_input_tokens":33}}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
        );
        let u = extract_last_assistant_usage(raw).unwrap();
        assert_eq!(u.input_tokens, 111);
        assert_eq!(u.output_tokens, 22);
        assert_eq!(u.cache_creation_input_tokens, 5);
        assert_eq!(u.cache_read_input_tokens, 33);
    }

    #[test]
    fn extract_last_assistant_usage_missing_fields_default_zero() {
        let raw = r#"{"type":"assistant","message":{"usage":{"input_tokens":7}}}"#;
        let u = extract_last_assistant_usage(raw).unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn extract_last_assistant_usage_none_when_absent_or_malformed() {
        assert!(extract_last_assistant_usage("").is_none());
        assert!(extract_last_assistant_usage("not json\n{also not}").is_none());
        assert!(
            extract_last_assistant_usage(r#"{"type":"user","message":{"content":"x"}}"#).is_none()
        );
    }
}
