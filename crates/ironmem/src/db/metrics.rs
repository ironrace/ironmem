//! Metrics storage layer — insert/query API for the four counter tables
//! introduced in migration 008 (`token_usage`, `occupancy_samples`,
//! `session_summary`, `task_outcomes`).
//!
//! The insert/upsert/`query_*` CRUD half is storage-only by design: it holds no
//! business logic or call-site wiring — callers construct and pass the typed
//! input structs. The `report_*` aggregate methods are a distinct
//! reporting/query surface: they encode the METRICS_SPEC §10 canonical queries
//! and the §2.3 task-identity policy (e.g. the `task_tag`→`collab_session_id`
//! alias for collab token rows) in SQL. That query policy intentionally lives
//! here, not in the report renderer — the renderer owns only shaping + §7 cost.
//! Enum column values are stringly-typed here; the DB CHECK constraints
//! (see `migrations/008_metrics.sql`) enforce domain correctness so a
//! malformed direct write cannot land an out-of-domain value.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::schema::Database;
use crate::error::MemoryError;

// ---------------------------------------------------------------------------
// Typed enums
// ---------------------------------------------------------------------------

/// Whether a terminal-state attestation actually landed on a `task_outcomes`
/// row.
///
/// [`Database::mark_task_outcome_done`] is best-effort by design — every
/// caller runs it after its protocol transaction has committed, so a metrics
/// failure can never roll back or fail a collab turn. Best-effort is not the
/// same as unobservable, though: a 0-row UPDATE means the ledger and the
/// session now disagree, and the caller is the only place that knows what that
/// disagreement means. `#[must_use]` because ignoring the answer is exactly
/// how the distinction was lost before this type existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a 0-row attestation leaves the metrics ledger disagreeing with the session; \
              decide what that means rather than discarding it"]
pub enum TaskOutcomeAttestation {
    /// The UPDATE matched the task's row and wrote the terminal state.
    Recorded,
    /// No row carried this `task_tag` — metrics were disabled when the task
    /// started, or the row was written under a different tag. Already warned
    /// at the storage layer; the caller decides whether it is worth more.
    NoRow,
}

/// Exploration-token verdict for one code-map MCP call (Phase 5 / issue #94).
/// The ONLY two legal `token_usage.map_status` values. Confining the wire
/// strings to the `ToSql`/`FromSql` impls below keeps `"map_hit"`/`"map_miss"`
/// from spreading as bare literals across the codebase; the DB CHECK constraint
/// (migration 011) remains as defense-in-depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MapStatus {
    /// A `code_map_load` returned a found map with verdict `fresh`.
    #[serde(rename = "map_hit")]
    Hit,
    /// Every other case — stale/rescout/absent load, or any `code_map_write`.
    #[serde(rename = "map_miss")]
    Miss,
}

impl MapStatus {
    /// The canonical DB/wire string — the single producer of these literals.
    pub fn as_str(self) -> &'static str {
        match self {
            MapStatus::Hit => "map_hit",
            MapStatus::Miss => "map_miss",
        }
    }
}

impl ToSql for MapStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for MapStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "map_hit" => Ok(MapStatus::Hit),
            "map_miss" => Ok(MapStatus::Miss),
            other => Err(FromSqlError::Other(
                format!("invalid map_status value: {other:?}").into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// Input for a new `token_usage` row (id is auto-assigned by SQLite).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTokenUsage {
    pub ts: String,
    pub source: String,
    pub harness: String,
    pub model: Option<String>,
    /// MCP tool name for `source='mcp_response'` rows. Non-tool protocol
    /// responses and pre-014 rows keep NULL.
    pub tool_name: Option<String>,
    pub session_id: Option<String>,
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub task_tag: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub estimated: bool,
    pub chars: i64,
    pub cost_usd: Option<f64>,
    /// Exploration-token attribution INPUT (Phase 5 / issue #94): the verdict to
    /// stamp on this new row. `None` for rows not participating in lazy
    /// code-map attribution.
    pub map_status: Option<MapStatus>,
    pub turn_id: Option<String>,
    pub area: Option<String>,
    /// Serialized JSON byte sizes observed when opt-in response compaction ran.
    /// Both are `None` when compaction was disabled or the response was ineligible.
    pub original_response_bytes: Option<i64>,
    pub compacted_response_bytes: Option<i64>,
}

/// Build a `NewTokenUsage` from an LLM call result. `source` is the call site
/// (`"llm_rerank"` | `"pref_extract"`); `ts` is an RFC3339 timestamp. `harness`
/// is fixed to `"claude"` because these are ironmem-internal Claude-model calls.
/// Context columns (`collab_session_id`, `collab_phase`, `task_tag`) are left
/// `None` here — callers apply attribution by chaining `.with_context(&ctx)`.
pub fn new_token_usage_from_llm(
    source: &str,
    resp: &ironrace_rerank::LlmResponse,
    ts: String,
) -> NewTokenUsage {
    NewTokenUsage {
        ts,
        source: source.to_string(),
        harness: "claude".to_string(),
        model: if resp.model.is_empty() {
            None
        } else {
            Some(resp.model.clone())
        },
        tool_name: None,
        session_id: None,
        collab_session_id: None,
        collab_phase: None,
        task_tag: None,
        input_tokens: resp.usage.input_tokens as i64,
        output_tokens: resp.usage.output_tokens as i64,
        cache_creation_input_tokens: resp.usage.cache_creation_input_tokens as i64,
        cache_read_input_tokens: resp.usage.cache_read_input_tokens as i64,
        estimated: resp.estimated,
        chars: resp.chars() as i64,
        cost_usd: resp.cost_usd,
        map_status: None,
        turn_id: None,
        area: None,
        original_response_bytes: None,
        compacted_response_bytes: None,
    }
}

impl NewTokenUsage {
    /// Build a `source='transcript'` row from a parsed transcript row plus the
    /// resolved attribution context. Centralizes the ~12 invariant columns
    /// (`source="transcript"`, `estimated=false`, `cost_usd=None`, `chars=0`,
    /// `map_status=None`, `area=None`) that the Claude and Codex hook branches
    /// would otherwise duplicate field-for-field. `harness` is the normalized
    /// `"claude"`/`"codex"`; `turn_id` carries the parser's idempotency key.
    pub(crate) fn from_transcript(
        row: crate::metrics::transcript::TranscriptRow,
        harness: &str,
        ts: String,
        session_id: Option<&str>,
        ctx: &crate::metrics::MetricsContext,
    ) -> NewTokenUsage {
        NewTokenUsage {
            ts,
            source: "transcript".to_string(),
            harness: harness.to_string(),
            model: row.model,
            tool_name: None,
            session_id: session_id.map(|s| s.to_string()),
            collab_session_id: ctx.collab_session_id.clone(),
            collab_phase: ctx.collab_phase.clone(),
            task_tag: ctx.task_tag.clone(),
            input_tokens: row.usage.input_tokens,
            output_tokens: row.usage.output_tokens,
            cache_creation_input_tokens: row.usage.cache_creation_input_tokens,
            cache_read_input_tokens: row.usage.cache_read_input_tokens,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: Some(row.turn_id),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        }
    }

    /// Return a copy stamped with the resolved attribution context
    /// (METRICS_SPEC §2.3/§3). Consuming builder: callers chain it after
    /// construction; the resolved context replaces all three attribution
    /// columns — a resolved context is authoritative and the `.or()` fallback
    /// was the only path that could produce a §2.3-violating both-set row.
    pub(crate) fn with_context(self, ctx: &crate::metrics::MetricsContext) -> NewTokenUsage {
        NewTokenUsage {
            collab_session_id: ctx.collab_session_id.clone(),
            collab_phase: ctx.collab_phase.clone(),
            task_tag: ctx.task_tag.clone(),
            ..self
        }
    }
}

/// A stored `token_usage` row including its auto-assigned `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub id: i64,
    pub ts: String,
    pub source: String,
    pub harness: String,
    pub model: Option<String>,
    pub tool_name: Option<String>,
    pub session_id: Option<String>,
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub task_tag: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub estimated: bool,
    pub chars: i64,
    pub cost_usd: Option<f64>,
    /// Exploration-token attribution as PERSISTED (Phase 5 / issue #94): the
    /// verdict read back from the stored row. `None` for rows not participating
    /// in lazy code-map attribution.
    pub map_status: Option<MapStatus>,
    pub turn_id: Option<String>,
    pub area: Option<String>,
    pub original_response_bytes: Option<i64>,
    pub compacted_response_bytes: Option<i64>,
}

/// Query filters for `token_usage`. All fields are optional; unset fields
/// match every row (see `query_token_usage` for ordering and limit behavior).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsageQuery {
    pub task_tag: Option<String>,
    pub collab_session_id: Option<String>,
    pub collab_phase: Option<String>,
    pub limit: Option<usize>,
}

/// Input for a new `occupancy_samples` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewOccupancySample {
    pub ts: String,
    pub harness: String,
    pub session_id: Option<String>,
    pub workspace_root: Option<String>,
    pub hook_event: Option<String>,
    pub input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub context_window: i64,
    pub occupancy_pct: Option<f64>,
}

/// A stored `occupancy_samples` row including its auto-assigned `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OccupancySample {
    pub id: i64,
    pub ts: String,
    pub harness: String,
    pub session_id: Option<String>,
    pub workspace_root: Option<String>,
    pub hook_event: Option<String>,
    pub input_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub context_window: i64,
    pub occupancy_pct: Option<f64>,
}

/// A `session_summary` row. `session_id` is the primary key and is
/// caller-supplied (not auto-assigned). This one struct is both the
/// `upsert_session_summary` input and the `get_session_summary` result — the
/// `New*`/stored split used for `token_usage`/`occupancy_samples` is
/// intentionally collapsed here because the PK is caller-supplied (there is no
/// auto-assigned `id` to omit on insert).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub harness: String,
    pub workspace_root: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub peak_occupancy_pct: Option<f64>,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub mcp_chars_served: i64,
    pub compactions: i64,
}

/// A `task_outcomes` row. `task_tag` is the unique caller-supplied key
/// (METRICS_SPEC §5.4); `id` is not exposed — callers query by `task_tag`.
/// This one struct is both the `upsert_task_outcome` input and the
/// `get_task_outcome` / `task_outcomes_for_collab` result (same rationale as
/// [`SessionSummary`]: the key is caller-supplied, so no `New*` split is
/// needed). Note: `task_tag` is the `NOT NULL UNIQUE` key sourced from
/// collab_start / status (METRICS_SPEC §5.4), so a writer keying a task by
/// collab session must derive a non-null `task_tag` before upserting (the
/// COALESCE-style task identity in §2.3 governs `token_usage` rollups, not the
/// `task_outcomes` persistence contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub task_tag: String,
    pub collab_session_id: Option<String>,
    pub started_at: Option<String>,
    pub done_at: Option<String>,
    pub outcome: Option<String>,
    pub review_rounds: i64,
    pub fix_commits: i64,
    pub handoffs: i64,
    pub pr_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Row mappers (private free functions)
// ---------------------------------------------------------------------------

fn map_token_usage(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenUsage> {
    let estimated_int: i64 = row.get(14)?;
    Ok(TokenUsage {
        id: row.get(0)?,
        ts: row.get(1)?,
        source: row.get(2)?,
        harness: row.get(3)?,
        model: row.get(4)?,
        tool_name: row.get(5)?,
        session_id: row.get(6)?,
        collab_session_id: row.get(7)?,
        collab_phase: row.get(8)?,
        task_tag: row.get(9)?,
        input_tokens: row.get(10)?,
        output_tokens: row.get(11)?,
        cache_creation_input_tokens: row.get(12)?,
        cache_read_input_tokens: row.get(13)?,
        estimated: estimated_int != 0,
        chars: row.get(15)?,
        cost_usd: row.get(16)?,
        map_status: row.get(17)?,
        turn_id: row.get(18)?,
        area: row.get(19)?,
        original_response_bytes: row.get(20)?,
        compacted_response_bytes: row.get(21)?,
    })
}

fn map_occupancy_sample(row: &rusqlite::Row<'_>) -> rusqlite::Result<OccupancySample> {
    Ok(OccupancySample {
        id: row.get(0)?,
        ts: row.get(1)?,
        harness: row.get(2)?,
        session_id: row.get(3)?,
        workspace_root: row.get(4)?,
        hook_event: row.get(5)?,
        input_tokens: row.get(6)?,
        cache_read_input_tokens: row.get(7)?,
        context_window: row.get(8)?,
        occupancy_pct: row.get(9)?,
    })
}

fn map_session_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        session_id: row.get(0)?,
        harness: row.get(1)?,
        workspace_root: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        peak_occupancy_pct: row.get(5)?,
        total_input_tokens: row.get(6)?,
        total_output_tokens: row.get(7)?,
        mcp_chars_served: row.get(8)?,
        compactions: row.get(9)?,
    })
}

fn map_task_outcome(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskOutcome> {
    Ok(TaskOutcome {
        task_tag: row.get(0)?,
        collab_session_id: row.get(1)?,
        started_at: row.get(2)?,
        done_at: row.get(3)?,
        outcome: row.get(4)?,
        review_rounds: row.get(5)?,
        fix_commits: row.get(6)?,
        handoffs: row.get(7)?,
        pr_url: row.get(8)?,
    })
}

// ---------------------------------------------------------------------------
// §10 report aggregate result structs + row mapper
// ---------------------------------------------------------------------------

/// One measured `token_usage` aggregate at (task_key, collab_phase, model,
/// harness) grain — the §10.1-compatible source the report rolls up to the
/// (task_key, collab_phase) grain (model/harness retained so §7 rates apply).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskPhaseModelTokens {
    pub task_key: String,
    pub collab_phase: Option<String>,
    pub model: Option<String>,
    pub harness: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub cache_read_input_tokens: i64,
    /// `SUM(cost_usd)` — `None` when every contributing row's `cost_usd` is NULL.
    pub provider_cost_usd: Option<f64>,
}

/// Phase-5 / issue #94 exploration-token attribution aggregate.
/// Produced by `report_exploration_delta`; one unit per DISTINCT non-NULL
/// `turn_id` over `token_usage` rows where `source = 'mcp_response'` and
/// `map_status IN ('map_hit', 'map_miss')`. Each turn gets a single verdict:
/// `map_hit` only when the turn has a hit and NO miss, else `map_miss`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplorationReport {
    /// Total distinct `turn_id` values with a tagged `map_status`.
    pub total_turns: i64,
    /// Distinct `turn_id` values whose per-turn verdict is `map_hit`
    /// (a hit and no miss across the turn's exploration rows).
    pub map_hit_turns: i64,
    /// Distinct `turn_id` values whose per-turn verdict is `map_miss`
    /// (at least one miss across the turn's exploration rows).
    pub map_miss_turns: i64,
    /// `map_hit_turns / total_turns`; `0.0` when `total_turns == 0`.
    pub hit_rate: f64,
    /// Mean `(input_tokens + output_tokens)` per turn for `map_hit` turns.
    ///
    /// **v0 note:** the MCP layer has no LLM-issued token counts for code-map
    /// calls; `account_mcp_response` uses the response-size estimate
    /// (`ceil(chars / 4)`) as `output_tokens` on the tagged MCP response row.
    pub mean_tokens_map_hit: f64,
    /// Mean `(input_tokens + output_tokens)` per turn for `map_miss` turns.
    ///
    /// See `mean_tokens_map_hit` — same v0 proxy applies.
    pub mean_tokens_map_miss: f64,
}

/// Repeated-context indicator: aggregate sizing of all `source='mcp_response'`
/// rows. `output_tokens` is the v0 response-size proxy (`ceil(chars / 4)`); a
/// large mean signals heavy repeated context flowing through MCP responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResponseSizing {
    /// Number of `source='mcp_response'` rows.
    pub row_count: i64,
    /// Σ `output_tokens` (response-size proxy) over those rows.
    pub total_output_tokens: i64,
    /// `total_output_tokens / row_count`; `0.0` when `row_count == 0`.
    pub mean_output_tokens: f64,
    /// Top response-size contributors grouped by collab session and MCP tool.
    pub top_tools: Vec<McpResponseToolSizing>,
    /// Complete response-size distributions grouped by harness and MCP tool.
    /// This is intentionally separate from `top_tools`: a regression gate must
    /// see every tool, not only the largest ten groups.
    pub distributions: Vec<McpResponseToolDistribution>,
}

/// Per-tool MCP response sizing. `collab_session_id` is NULL for non-collab
/// traffic; `tool_name` is NULL for protocol-level responses such as
/// `initialize` or `tools/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResponseToolSizing {
    pub collab_session_id: Option<String>,
    pub tool_name: Option<String>,
    pub row_count: i64,
    pub total_chars: i64,
    pub total_output_tokens: i64,
    pub mean_output_tokens: f64,
}

/// Response-size distribution for one `(harness, tool_name)` group.
/// Percentiles use the nearest-rank rule over `output_tokens`, which keeps the
/// result deterministic and avoids inventing an interpolated token count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResponseToolDistribution {
    pub harness: String,
    pub tool_name: Option<String>,
    pub row_count: i64,
    pub total_chars: i64,
    pub total_output_tokens: i64,
    pub mean_output_tokens: f64,
    pub p50_output_tokens: i64,
    pub p95_output_tokens: i64,
    pub max_output_tokens: i64,
}

fn nearest_rank_percentile(sorted: &[i64], percentile: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() as f64 * percentile).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Repeated-context indicator: how many turns the transcript-token capture
/// covers (`source='transcript'` rows). Higher coverage means richer per-turn
/// token data underpinning the value summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptCoverage {
    /// Distinct `turn_id` values with a `source='transcript'` row.
    pub turn_count: i64,
    /// Σ `input_tokens + output_tokens` over `source='transcript'` rows.
    pub total_tokens: i64,
}

/// METRICS_SPEC §10.2 split row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEstimatedSplit {
    pub task_key: String,
    pub estimated: bool,
    pub tokens: i64,
}

/// METRICS_SPEC §10.4 headline row (also used for the non-completion variant).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeadlineTokens {
    pub task_tag: String,
    pub collab_session_id: Option<String>,
    pub tokens_to_done: i64,
    pub provider_cost_usd: Option<f64>,
}

fn map_task_phase_model_tokens(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskPhaseModelTokens> {
    Ok(TaskPhaseModelTokens {
        task_key: row.get(0)?,
        collab_phase: row.get(1)?,
        model: row.get(2)?,
        harness: row.get(3)?,
        input_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
        output_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
        cache_creation_input_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        cache_read_input_tokens: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
        provider_cost_usd: row.get(8)?,
    })
}

// ---------------------------------------------------------------------------
// impl Database
// ---------------------------------------------------------------------------

impl Database {
    /// Insert a `token_usage` row and return its auto-assigned rowid.
    pub fn insert_token_usage(&self, row: &NewTokenUsage) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO token_usage (
                ts, source, harness, model, tool_name, session_id, collab_session_id, collab_phase,
                task_tag, input_tokens, output_tokens, cache_creation_input_tokens,
                cache_read_input_tokens, estimated, chars, cost_usd,
                map_status, turn_id, area, original_response_bytes, compacted_response_bytes
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                row.ts,
                row.source,
                row.harness,
                row.model,
                row.tool_name,
                row.session_id,
                row.collab_session_id,
                row.collab_phase,
                row.task_tag,
                row.input_tokens,
                row.output_tokens,
                row.cache_creation_input_tokens,
                row.cache_read_input_tokens,
                row.estimated as i64,
                row.chars,
                row.cost_usd,
                row.map_status,
                row.turn_id,
                row.area,
                row.original_response_bytes,
                row.compacted_response_bytes,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Query `token_usage` rows with optional filters, ordered by `(ts, id)`.
    ///
    /// Uses the `(?N IS NULL OR col = ?N)` idiom so a single fixed SQL string
    /// handles all filter combinations without dynamic query building.
    /// `limit` of `None` maps to `-1` which SQLite treats as unlimited.
    pub fn query_token_usage(&self, q: &TokenUsageQuery) -> Result<Vec<TokenUsage>, MemoryError> {
        let limit = q.limit.map(|l| l as i64).unwrap_or(-1);
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, source, harness, model, tool_name, session_id, collab_session_id, collab_phase,
                    task_tag, input_tokens, output_tokens, cache_creation_input_tokens,
                    cache_read_input_tokens, estimated, chars, cost_usd,
                    map_status, turn_id, area, original_response_bytes, compacted_response_bytes
             FROM token_usage
             WHERE (?1 IS NULL OR task_tag = ?1)
               AND (?2 IS NULL OR collab_session_id = ?2)
               AND (?3 IS NULL OR collab_phase = ?3)
             ORDER BY ts, id
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![q.task_tag, q.collab_session_id, q.collab_phase, limit],
            map_token_usage,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// Insert an `occupancy_samples` row and return its auto-assigned rowid.
    pub fn insert_occupancy_sample(&self, row: &NewOccupancySample) -> Result<i64, MemoryError> {
        self.conn.execute(
            "INSERT INTO occupancy_samples (
                ts, harness, session_id, workspace_root, hook_event,
                input_tokens, cache_read_input_tokens, context_window, occupancy_pct
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                row.ts,
                row.harness,
                row.session_id,
                row.workspace_root,
                row.hook_event,
                row.input_tokens,
                row.cache_read_input_tokens,
                row.context_window,
                row.occupancy_pct,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Return up to `limit` occupancy samples for `session_id`, ordered by
    /// `(ts, id)` ascending.
    pub fn occupancy_samples_for_session(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<OccupancySample>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, harness, session_id, workspace_root, hook_event,
                    input_tokens, cache_read_input_tokens, context_window, occupancy_pct
             FROM occupancy_samples
             WHERE session_id = ?1
             ORDER BY ts, id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit as i64], map_occupancy_sample)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// Insert or replace a `session_summary` row. On conflict the non-key
    /// columns are updated to the new values (full-row upsert).
    pub fn upsert_session_summary(&self, row: &SessionSummary) -> Result<(), MemoryError> {
        self.conn.execute(
            "INSERT INTO session_summary (
                session_id, harness, workspace_root, started_at, ended_at,
                peak_occupancy_pct, total_input_tokens, total_output_tokens,
                mcp_chars_served, compactions
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
            ON CONFLICT(session_id) DO UPDATE SET
                harness             = excluded.harness,
                workspace_root      = excluded.workspace_root,
                started_at          = excluded.started_at,
                ended_at            = excluded.ended_at,
                peak_occupancy_pct  = excluded.peak_occupancy_pct,
                total_input_tokens  = excluded.total_input_tokens,
                total_output_tokens = excluded.total_output_tokens,
                mcp_chars_served    = excluded.mcp_chars_served,
                compactions         = excluded.compactions",
            params![
                row.session_id,
                row.harness,
                row.workspace_root,
                row.started_at,
                row.ended_at,
                row.peak_occupancy_pct,
                row.total_input_tokens,
                row.total_output_tokens,
                row.mcp_chars_served,
                row.compactions,
            ],
        )?;
        Ok(())
    }

    /// Atomically merge a `session_summary` delta in a single statement, so the
    /// MCP-server process and the hook process can co-key the same row without a
    /// cross-process read-modify-write race (a get→mutate→clobber sequence on
    /// separate connections silently drops one writer's increment under WAL).
    ///
    /// The `delta` carries each caller's OWN increment: additive columns
    /// (`mcp_chars_served`, `total_input_tokens`, `total_output_tokens`,
    /// `compactions`) are summed engine-side; `peak_occupancy_pct` takes the
    /// running max; `started_at` is set-once (earliest wins via COALESCE);
    /// `ended_at` takes the newest non-null; `harness` takes the latest;
    /// `workspace_root` keeps the latest non-null.
    pub fn accumulate_session_summary(&self, delta: &SessionSummary) -> Result<(), MemoryError> {
        self.conn.execute(
            "INSERT INTO session_summary (
                session_id, harness, workspace_root, started_at, ended_at,
                peak_occupancy_pct, total_input_tokens, total_output_tokens,
                mcp_chars_served, compactions
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
            ON CONFLICT(session_id) DO UPDATE SET
                harness             = excluded.harness,
                workspace_root      = COALESCE(excluded.workspace_root, session_summary.workspace_root),
                started_at          = COALESCE(session_summary.started_at, excluded.started_at),
                ended_at            = COALESCE(excluded.ended_at, session_summary.ended_at),
                peak_occupancy_pct  = MAX(
                                        COALESCE(session_summary.peak_occupancy_pct, excluded.peak_occupancy_pct),
                                        COALESCE(excluded.peak_occupancy_pct, session_summary.peak_occupancy_pct)
                                      ),
                total_input_tokens  = session_summary.total_input_tokens  + excluded.total_input_tokens,
                total_output_tokens = session_summary.total_output_tokens + excluded.total_output_tokens,
                mcp_chars_served    = session_summary.mcp_chars_served     + excluded.mcp_chars_served,
                compactions         = session_summary.compactions         + excluded.compactions",
            params![
                delta.session_id,
                delta.harness,
                delta.workspace_root,
                delta.started_at,
                delta.ended_at,
                delta.peak_occupancy_pct,
                delta.total_input_tokens,
                delta.total_output_tokens,
                delta.mcp_chars_served,
                delta.compactions,
            ],
        )?;
        Ok(())
    }

    /// Fetch a `session_summary` by `session_id`. Returns `None` if not found.
    pub fn get_session_summary(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionSummary>, MemoryError> {
        self.conn
            .query_row(
                "SELECT session_id, harness, workspace_root, started_at, ended_at,
                        peak_occupancy_pct, total_input_tokens, total_output_tokens,
                        mcp_chars_served, compactions
                 FROM session_summary
                 WHERE session_id = ?1",
                params![session_id],
                map_session_summary,
            )
            .optional()
            .map_err(MemoryError::from)
    }

    /// Insert or replace a `task_outcomes` row. On conflict the non-key
    /// columns are updated to the new values (full-row upsert).
    pub fn upsert_task_outcome(&self, row: &TaskOutcome) -> Result<(), MemoryError> {
        self.conn.execute(
            "INSERT INTO task_outcomes (
                task_tag, collab_session_id, started_at, done_at, outcome,
                review_rounds, fix_commits, handoffs, pr_url
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
            ON CONFLICT(task_tag) DO UPDATE SET
                collab_session_id = excluded.collab_session_id,
                started_at        = excluded.started_at,
                done_at           = excluded.done_at,
                outcome           = excluded.outcome,
                review_rounds     = excluded.review_rounds,
                fix_commits       = excluded.fix_commits,
                handoffs          = excluded.handoffs,
                pr_url            = excluded.pr_url",
            params![
                row.task_tag,
                row.collab_session_id,
                row.started_at,
                row.done_at,
                row.outcome,
                row.review_rounds,
                row.fix_commits,
                row.handoffs,
                row.pr_url,
            ],
        )?;
        Ok(())
    }

    /// Fetch a `task_outcomes` row by `task_tag`. Returns `None` if not found.
    pub fn get_task_outcome(&self, task_tag: &str) -> Result<Option<TaskOutcome>, MemoryError> {
        self.conn
            .query_row(
                "SELECT task_tag, collab_session_id, started_at, done_at, outcome,
                        review_rounds, fix_commits, handoffs, pr_url
                 FROM task_outcomes
                 WHERE task_tag = ?1",
                params![task_tag],
                map_task_outcome,
            )
            .optional()
            .map_err(MemoryError::from)
    }

    /// Atomically bump `handoffs` for one task. A single UPDATE so concurrent
    /// writer processes can't lose increments to a read-modify-write race.
    /// Missing `task_tag` is a no-op `Ok` — the metrics layer is best-effort
    /// and a missing row is a caller problem, not a transport error.
    /// Never creates a stub row (a collab row needs its full identity incl.
    /// `collab_session_id`; a stub keyed only by `task_tag` would be
    /// invisible/incomplete in metrics).
    pub fn increment_task_handoffs(&self, task_tag: &str) -> Result<(), MemoryError> {
        let changed = self.conn.execute(
            "UPDATE task_outcomes SET handoffs = handoffs + 1 WHERE task_tag = ?1",
            params![task_tag],
        )?;
        if changed == 0 {
            tracing::warn!(
                task_tag = %task_tag,
                operation = "increment_task_handoffs",
                "metrics: UPDATE matched 0 rows — task_tag not found in task_outcomes"
            );
        }
        Ok(())
    }

    /// Atomically bump `review_rounds` for one task (METRICS_SPEC §4). A
    /// single UPDATE so concurrent writer processes can't lose increments to
    /// a read-modify-write race. Missing `task_tag` is a no-op `Ok` — the
    /// metrics layer is best-effort and a missing row is a caller problem,
    /// not a transport error.
    pub fn increment_task_review_rounds(&self, task_tag: &str) -> Result<(), MemoryError> {
        let changed = self.conn.execute(
            "UPDATE task_outcomes SET review_rounds = review_rounds + 1 WHERE task_tag = ?1",
            params![task_tag],
        )?;
        if changed == 0 {
            tracing::warn!(
                task_tag = %task_tag,
                operation = "increment_task_review_rounds",
                "metrics: UPDATE matched 0 rows — task_tag not found in task_outcomes"
            );
        }
        Ok(())
    }

    /// Partial terminal-state update for one task (METRICS_SPEC §5.4):
    /// only non-`None` fields are written (COALESCE keeps existing values),
    /// counters are never touched. Missing `task_tag` is a no-op `Ok`.
    ///
    /// Returns which of the two happened. It used to return `()`, so the
    /// no-row case — metrics disabled when the session started, a row written
    /// under a different `task_tag`, a row never created at all — was
    /// indistinguishable from a recorded attestation at every call site, and
    /// the only trace was the `warn` below. That is tolerable for the callers
    /// that merely *stamp* an outcome; it is not for `collab_end`'s abandon
    /// arm, which is the one caller that overwrites an outcome another path
    /// already wrote and the one whose session no later surface can re-attest,
    /// because the seal refuses every mutating call on it afterwards. Giving
    /// the caller the fact lets it say so in its own terms.
    pub fn mark_task_outcome_done(
        &self,
        task_tag: &str,
        done_at: Option<&str>,
        outcome: Option<&str>,
        pr_url: Option<&str>,
    ) -> Result<TaskOutcomeAttestation, MemoryError> {
        let changed = self.conn.execute(
            "UPDATE task_outcomes SET
                done_at = COALESCE(?2, done_at),
                outcome = COALESCE(?3, outcome),
                pr_url  = COALESCE(?4, pr_url)
             WHERE task_tag = ?1",
            params![task_tag, done_at, outcome, pr_url],
        )?;
        if changed == 0 {
            tracing::warn!(
                task_tag = %task_tag,
                operation = "mark_task_outcome_done",
                "metrics: UPDATE matched 0 rows — task_tag not found in task_outcomes"
            );
            return Ok(TaskOutcomeAttestation::NoRow);
        }
        Ok(TaskOutcomeAttestation::Recorded)
    }

    /// Clear a stale `outcome='failed'`/`done_at` row written by an earlier
    /// terminal `failure_report`, so a `collab_resume`d session can complete
    /// normally afterward (METRICS_SPEC §5.4 amendment, task 10). Unlike
    /// `mark_task_outcome_done`'s `COALESCE` semantics (which can only ever
    /// set a value, never null one back out), this issues a genuine clear.
    ///
    /// Scoped to `outcome = 'failed'` in the WHERE clause — a defensive guard
    /// against clobbering a genuinely different terminal state
    /// (`merged`/`abandoned`) if this is ever called against the wrong
    /// `task_tag`. A 0-row match is not necessarily an error: metrics may
    /// have been disabled when the original `failure_report` fired, so no
    /// row was ever marked failed in the first place. Logged at debug rather
    /// than `mark_task_outcome_done`'s warn, since that guards a different
    /// invariant (a task_tag that should exist but doesn't).
    pub fn clear_failed_task_outcome(&self, task_tag: &str) -> Result<(), MemoryError> {
        let changed = self.conn.execute(
            "UPDATE task_outcomes SET outcome = NULL, done_at = NULL
             WHERE task_tag = ?1 AND outcome = 'failed'",
            params![task_tag],
        )?;
        tracing::debug!(
            task_tag = %task_tag,
            rows_changed = changed,
            "metrics: clear_failed_task_outcome"
        );
        Ok(())
    }

    /// Return all `task_outcomes` rows for a given `collab_session_id`,
    /// ordered by `(started_at, id)` ascending.
    pub fn task_outcomes_for_collab(
        &self,
        collab_session_id: &str,
    ) -> Result<Vec<TaskOutcome>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_tag, collab_session_id, started_at, done_at, outcome,
                    review_rounds, fix_commits, handoffs, pr_url
             FROM task_outcomes
             WHERE collab_session_id = ?1
             ORDER BY started_at, id",
        )?;
        let rows = stmt.query_map(params![collab_session_id], map_task_outcome)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// METRICS_SPEC §10.1 (tokens-to-done per task, by phase; measured only) as a
    /// §10.1-COMPATIBLE roll-up: this GROUPs additionally by `model` and `harness`
    /// so the report can apply §7 per-model rates. Rolling the rows up to the
    /// (task_key, collab_phase) grain reproduces §10.1 exactly (SUM is associative);
    /// see the drift-guard test. NOT the literal §10.1 GROUP BY.
    /// `task` filters `COALESCE(collab_session_id, task_tag)` and also accepts
    /// a `task_outcomes.task_tag` alias for collab-token rows keyed only by
    /// `collab_session_id`; `since` filters `julianday(ts) >= julianday(?)` — a
    /// deliberate INSTANT comparison (not a lexical `ts >= ?` text compare) so a
    /// stored `+00:00`-offset `ts` and a normalized-`Z` `since` are equal at the
    /// same instant (METRICS_SPEC §12). Do not "optimize" back to text compare.
    pub fn report_tokens_by_task_phase(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<TaskPhaseModelTokens>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(collab_session_id, task_tag) AS task_key,
                    collab_phase, model, harness,
                    SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_input_tokens), SUM(cache_read_input_tokens),
                    SUM(cost_usd)
             FROM token_usage
             WHERE estimated = 0
               AND COALESCE(collab_session_id, task_tag) IS NOT NULL
               AND (?1 IS NULL
                    OR COALESCE(collab_session_id, task_tag) = ?1
                    OR COALESCE(collab_session_id, task_tag) IN (
                        SELECT collab_session_id FROM task_outcomes
                        WHERE task_tag = ?1 AND collab_session_id IS NOT NULL
                    ))
               AND (?2 IS NULL OR julianday(ts) >= julianday(?2))
             GROUP BY task_key, collab_phase, model, harness
             ORDER BY task_key, collab_phase, model, harness",
        )?;
        let rows = stmt.query_map(params![task, since], map_task_phase_model_tokens)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// METRICS_SPEC §10.2 measured-vs-estimated split per task (verbatim shape).
    pub fn report_measured_estimated_split(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<TaskEstimatedSplit>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(collab_session_id, task_tag) AS task_key,
                    estimated,
                    SUM(input_tokens + output_tokens + cache_creation_input_tokens + cache_read_input_tokens) AS tokens
             FROM token_usage
             WHERE COALESCE(collab_session_id, task_tag) IS NOT NULL
               AND (?1 IS NULL
                    OR COALESCE(collab_session_id, task_tag) = ?1
                    OR COALESCE(collab_session_id, task_tag) IN (
                        SELECT collab_session_id FROM task_outcomes
                        WHERE task_tag = ?1 AND collab_session_id IS NOT NULL
                    ))
               AND (?2 IS NULL OR julianday(ts) >= julianday(?2))
             GROUP BY task_key, estimated
             ORDER BY task_key, estimated",
        )?;
        let rows = stmt.query_map(params![task, since], |r| {
            Ok(TaskEstimatedSplit {
                task_key: r.get(0)?,
                estimated: r.get::<_, i64>(1)? != 0,
                tokens: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// METRICS_SPEC §10.3 iteration counts & outcome per task (verbatim; ORDER BY
    /// started_at). `task` matches `task_tag` OR `collab_session_id`; `since`
    /// filters `julianday(started_at) >= julianday(?)`. Reuses `map_task_outcome`.
    pub fn report_task_outcomes(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<TaskOutcome>, MemoryError> {
        let mut stmt = self.conn.prepare(
            "SELECT task_tag, collab_session_id, started_at, done_at, outcome,
                    review_rounds, fix_commits, handoffs, pr_url
             FROM task_outcomes
             WHERE (?1 IS NULL OR task_tag = ?1 OR collab_session_id = ?1)
               AND (?2 IS NULL OR julianday(started_at) >= julianday(?2))
             ORDER BY started_at, id",
        )?;
        let rows = stmt.query_map(params![task, since], map_task_outcome)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }

    /// METRICS_SPEC §10.4 headline tokens-to-done (merged only; verbatim JOIN).
    /// `since` filters the TOKEN side (`julianday(u.ts) >= julianday(?)`);
    /// `task` matches the outcome's `task_tag` OR `collab_session_id`. The
    /// OR-join is safe under the §10 uniqueness invariant (task_tag and
    /// collab_session_id each unique per task).
    pub fn report_headline(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<HeadlineTokens>, MemoryError> {
        self.headline_inner("t.outcome = 'merged'", task, since)
    }

    /// METRICS_SPEC §10.4 non-completion variant (`failed`/`abandoned`), §2.2/§9.4.
    pub fn report_non_completions(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<HeadlineTokens>, MemoryError> {
        self.headline_inner("t.outcome IN ('failed','abandoned')", task, since)
    }

    /// Phase-5 / issue #94: DIRECT-RECORD / TEST-SEEDING helper that writes one
    /// exploration-attribution row (`source = 'mcp_response'`, `estimated =
    /// false`) with explicit token counts. This is **NOT** the live MCP path:
    /// the live path tags the *estimated* `mcp_response` row produced by
    /// [`crate::metrics::account_mcp_response`] (driven from
    /// `mcp/server.rs::request_exploration_context`), using the response-size
    /// token proxy. This helper exists only to seed deterministic, known-token
    /// rows in tests; it is `#[cfg(test)]`-gated so it can never be mistaken for
    /// production wiring. All `NewTokenUsage` columns not covered by the
    /// parameters default to `None` or zero.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn record_exploration_tokens(
        &self,
        ts: &str,
        harness: &str,
        input_tokens: i64,
        output_tokens: i64,
        map_status: Option<MapStatus>,
        turn_id: Option<&str>,
        area: Option<&str>,
    ) -> Result<i64, MemoryError> {
        self.insert_token_usage(&NewTokenUsage {
            ts: ts.to_string(),
            source: "mcp_response".to_string(),
            harness: harness.to_string(),
            model: None,
            tool_name: None,
            session_id: None,
            collab_session_id: None,
            collab_phase: None,
            task_tag: None,
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status,
            turn_id: turn_id.map(|s| s.to_string()),
            area: area.map(|s| s.to_string()),
            original_response_bytes: None,
            compacted_response_bytes: None,
        })
    }

    /// Phase-5 / issue #94: aggregate exploration-token attribution across all
    /// `mcp_response` rows that carry a tagged `map_status`. Produces the
    /// `ExplorationReport` used by the §10 Phase-5 report section.
    ///
    /// Groups by `(turn_id, map_status)` so multiple rows for the same turn
    /// (e.g. retries) collapse to a single per-turn token total.
    pub fn report_exploration_delta(&self) -> Result<ExplorationReport, MemoryError> {
        self.report_exploration_delta_for(None, None)
    }

    /// Scoped variant of [`Self::report_exploration_delta`]. The task predicate
    /// follows the report's `collab_session_id`/`task_tag` alias semantics.
    pub fn report_exploration_delta_for(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<ExplorationReport, MemoryError> {
        // One row per DISTINCT turn_id. A turn's tokens are summed across all its
        // exploration rows; its verdict is a single value derived per turn:
        // `map_hit` only if the turn has a hit and NO miss, else `map_miss`.
        // This avoids two skews: (a) a turn emitting both a hit and a miss being
        // double-counted as two turns, and (b) NULL turn_id rows collapsing into
        // one group — we exclude NULL turn_id entirely since an untagged row
        // cannot be attributed to a turn.
        const FILTER: &str = "source = 'mcp_response'
               AND (?1 IS NULL
                    OR COALESCE(collab_session_id, task_tag) = ?1
                    OR collab_session_id IN (
                        SELECT collab_session_id FROM task_outcomes
                        WHERE task_tag = ?1 AND collab_session_id IS NOT NULL
                    ))
               AND (?2 IS NULL OR julianday(ts) >= julianday(?2))";
        let mut stmt = self.conn.prepare(&format!(
            "SELECT
                 CASE WHEN SUM(map_status = 'map_miss') > 0 THEN 'map_miss'
                      ELSE 'map_hit' END AS turn_status,
                 SUM(input_tokens + output_tokens) AS total_tokens
             FROM token_usage
             WHERE {FILTER}
               AND turn_id IS NOT NULL
               AND map_status IN ('map_hit', 'map_miss')
             GROUP BY turn_id"
        ))?;
        struct TurnRow {
            map_status: String,
            total_tokens: i64,
        }
        let rows: Vec<TurnRow> = stmt
            .query_map(params![task, since], |r| {
                Ok(TurnRow {
                    map_status: r.get(0)?,
                    total_tokens: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)?;

        let mut map_hit_turns: i64 = 0;
        let mut map_miss_turns: i64 = 0;
        let mut sum_hit: f64 = 0.0;
        let mut sum_miss: f64 = 0.0;

        for row in &rows {
            if row.map_status == "map_hit" {
                map_hit_turns += 1;
                sum_hit += row.total_tokens as f64;
            } else {
                map_miss_turns += 1;
                sum_miss += row.total_tokens as f64;
            }
        }

        let total_turns = map_hit_turns + map_miss_turns;
        let hit_rate = if total_turns == 0 {
            0.0
        } else {
            map_hit_turns as f64 / total_turns as f64
        };
        let mean_tokens_map_hit = if map_hit_turns == 0 {
            0.0
        } else {
            sum_hit / map_hit_turns as f64
        };
        let mean_tokens_map_miss = if map_miss_turns == 0 {
            0.0
        } else {
            sum_miss / map_miss_turns as f64
        };

        Ok(ExplorationReport {
            total_turns,
            map_hit_turns,
            map_miss_turns,
            hit_rate,
            mean_tokens_map_hit,
            mean_tokens_map_miss,
        })
    }

    /// Aggregate sizing over all `source='mcp_response'` rows (issue #145).
    /// Read-only; no schema dependency beyond the existing `token_usage` table.
    pub fn report_mcp_response_sizing(&self) -> Result<McpResponseSizing, MemoryError> {
        self.report_mcp_response_sizing_for(None, None)
    }

    /// Aggregate MCP response sizing, optionally restricted to one report task
    /// and/or a report `--since` instant. The task filter follows the report's
    /// `collab_session_id`/`task_tag` alias semantics and is applied to all three
    /// aggregate views so a captured reference session is isolated from
    /// unrelated local traffic.
    pub fn report_mcp_response_sizing_for(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<McpResponseSizing, MemoryError> {
        const FILTER: &str = "source = 'mcp_response'
             AND (?1 IS NULL
                  OR COALESCE(collab_session_id, task_tag) = ?1
                  OR collab_session_id IN (
                      SELECT collab_session_id FROM task_outcomes
                      WHERE task_tag = ?1 AND collab_session_id IS NOT NULL
                  ))
             AND (?2 IS NULL OR julianday(ts) >= julianday(?2))";
        // Keep the three report views on one SQLite read snapshot. Without this,
        // a concurrent MCP write could make the top-level totals disagree with
        // the per-tool distributions returned in the same JSON report.
        let tx = self.conn.unchecked_transaction()?;

        let (row_count, total_output_tokens) = tx.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(output_tokens), 0) FROM token_usage WHERE {FILTER}"
            ),
            params![task, since],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        let mean_output_tokens = if row_count == 0 {
            0.0
        } else {
            total_output_tokens as f64 / row_count as f64
        };
        let top_sql = format!(
            "SELECT collab_session_id,
                    tool_name,
                    COUNT(*) AS row_count,
                    COALESCE(SUM(chars), 0) AS total_chars,
                    COALESCE(SUM(output_tokens), 0) AS total_output_tokens
             FROM token_usage
             WHERE {FILTER}
             GROUP BY collab_session_id, tool_name
             ORDER BY total_output_tokens DESC, total_chars DESC, row_count DESC,
                      collab_session_id ASC, tool_name ASC
             LIMIT 10"
        );
        let mut stmt = tx.prepare(&top_sql)?;
        let top_tools = stmt
            .query_map(params![task, since], |r| {
                let rows = r.get::<_, i64>(2)?;
                let total = r.get::<_, i64>(4)?;
                Ok(McpResponseToolSizing {
                    collab_session_id: r.get(0)?,
                    tool_name: r.get(1)?,
                    row_count: rows,
                    total_chars: r.get(3)?,
                    total_output_tokens: total,
                    mean_output_tokens: if rows == 0 {
                        0.0
                    } else {
                        total as f64 / rows as f64
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)?;

        let mut raw: std::collections::BTreeMap<(String, Option<String>), (Vec<i64>, i64)> =
            std::collections::BTreeMap::new();
        let mut stmt = tx.prepare(&format!(
            "SELECT harness, tool_name, output_tokens, chars
             FROM token_usage
             WHERE {FILTER}
             ORDER BY harness ASC, tool_name ASC, id ASC"
        ))?;
        let rows = stmt.query_map(params![task, since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (harness, tool_name, output_tokens, chars) = row?;
            let (tokens, total_chars) = raw
                .entry((harness, tool_name))
                .or_insert_with(|| (Vec::new(), 0));
            tokens.push(output_tokens);
            *total_chars += chars;
        }
        let distributions = raw
            .into_iter()
            .map(|((harness, tool_name), (mut tokens, total_chars))| {
                tokens.sort_unstable();
                let row_count = tokens.len() as i64;
                let total_output_tokens = tokens.iter().sum();
                McpResponseToolDistribution {
                    harness,
                    tool_name,
                    row_count,
                    total_chars,
                    total_output_tokens,
                    mean_output_tokens: total_output_tokens as f64 / row_count as f64,
                    p50_output_tokens: nearest_rank_percentile(&tokens, 0.50),
                    p95_output_tokens: nearest_rank_percentile(&tokens, 0.95),
                    max_output_tokens: *tokens.last().unwrap_or(&0),
                }
            })
            .collect();
        Ok(McpResponseSizing {
            row_count,
            total_output_tokens,
            mean_output_tokens,
            top_tools,
            distributions,
        })
    }

    /// Transcript-token coverage over `source='transcript'` rows (issue #145):
    /// distinct covered turns + summed `input + output` tokens. Read-only.
    pub fn report_transcript_coverage(&self) -> Result<TranscriptCoverage, MemoryError> {
        self.report_transcript_coverage_for(None, None)
    }

    /// Scoped variant of [`Self::report_transcript_coverage`]. The task
    /// predicate follows the report's `collab_session_id`/`task_tag` alias
    /// semantics.
    pub fn report_transcript_coverage_for(
        &self,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<TranscriptCoverage, MemoryError> {
        const FILTER: &str = "source = 'transcript'
             AND (?1 IS NULL
                  OR COALESCE(collab_session_id, task_tag) = ?1
                  OR collab_session_id IN (
                      SELECT collab_session_id FROM task_outcomes
                      WHERE task_tag = ?1 AND collab_session_id IS NOT NULL
                  ))
             AND (?2 IS NULL OR julianday(ts) >= julianday(?2))";
        let (turn_count, total_tokens) = self.conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT turn_id),
                    COALESCE(SUM(input_tokens + output_tokens), 0)
             FROM token_usage
             WHERE {FILTER} AND turn_id IS NOT NULL"
            ),
            params![task, since],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(TranscriptCoverage {
            turn_count,
            total_tokens,
        })
    }

    /// Idempotent upsert for a `source='transcript'` token_usage row (METRICS_SPEC
    /// §12 / Tasks 4+5). Inside one transaction:
    /// - `SELECT id FROM token_usage WHERE source='transcript' AND turn_id=?`
    /// - If found: UPDATE the four token components, `model`, `collab_session_id`,
    ///   `collab_phase`, `task_tag` on that row.
    /// - If not found: INSERT a new row with `source='transcript'`,
    ///   `estimated=false`, `cost_usd=NULL`.
    ///
    /// Scoping dedup to `source='transcript'` leaves `mcp_response`/`llm_rerank`/
    /// `pref_extract` rows untouched even when they share the same `turn_id`.
    pub fn upsert_transcript_token_usage(&self, row: &NewTokenUsage) -> Result<(), MemoryError> {
        self.upsert_transcript_token_usage_many(std::slice::from_ref(row))
    }

    /// Batch variant for full-transcript hook writes. Uses one immediate
    /// transaction for all parsed rows so overlapping hooks are serialized once
    /// and long Claude transcripts do not pay one lock/lookup cycle per message.
    pub fn upsert_transcript_token_usage_many(
        &self,
        rows: &[NewTokenUsage],
    ) -> Result<(), MemoryError> {
        if rows.is_empty() {
            return Ok(());
        }
        // `stop` and `precompact` can overlap in separate hook processes. A
        // deferred transaction allows both writers to observe "no row" before
        // either inserts; `BEGIN IMMEDIATE` serializes that SELECT/INSERT window
        // without requiring a schema migration.
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(), MemoryError> {
            for row in rows {
                self.upsert_transcript_token_usage_in_tx(row)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn upsert_transcript_token_usage_in_tx(&self, row: &NewTokenUsage) -> Result<(), MemoryError> {
        if row.source != "transcript" {
            return Err(MemoryError::Validation(
                "upsert_transcript_token_usage requires source='transcript'".to_string(),
            ));
        }
        if row.turn_id.is_none() {
            return Err(MemoryError::Validation(
                "upsert_transcript_token_usage requires turn_id".to_string(),
            ));
        }

        let existing_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM token_usage WHERE source = 'transcript' AND turn_id = ?1",
                params![row.turn_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;

        if let Some(id) = existing_id {
            self.conn.execute(
                "UPDATE token_usage SET
                    input_tokens = ?2,
                    output_tokens = ?3,
                    cache_creation_input_tokens = ?4,
                    cache_read_input_tokens = ?5,
                    model = ?6,
                    collab_session_id = ?7,
                    collab_phase = ?8,
                    task_tag = ?9
                 WHERE id = ?1",
                params![
                    id,
                    row.input_tokens,
                    row.output_tokens,
                    row.cache_creation_input_tokens,
                    row.cache_read_input_tokens,
                    row.model,
                    row.collab_session_id,
                    row.collab_phase,
                    row.task_tag,
                ],
            )?;
        } else {
            self.insert_token_usage(row)?;
        }
        Ok(())
    }

    fn headline_inner(
        &self,
        outcome_pred: &str,
        task: Option<&str>,
        since: Option<&str>,
    ) -> Result<Vec<HeadlineTokens>, MemoryError> {
        let sql = format!(
            "SELECT t.task_tag, t.collab_session_id,
                    SUM(u.input_tokens + u.output_tokens
                        + u.cache_creation_input_tokens + u.cache_read_input_tokens) AS tokens_to_done,
                    SUM(u.cost_usd) AS cost_usd
             FROM task_outcomes t
             JOIN token_usage u
               ON u.task_tag = t.task_tag OR u.collab_session_id = t.collab_session_id
             WHERE {outcome_pred}
               AND u.estimated = 0
               AND (?1 IS NULL OR t.task_tag = ?1 OR t.collab_session_id = ?1)
               AND (?2 IS NULL OR julianday(u.ts) >= julianday(?2))
             GROUP BY t.task_tag
             ORDER BY t.task_tag",
        );
        // `outcome_pred` is a fixed internal literal (never user input) — no injection surface.
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![task, since], |r| {
            Ok(HeadlineTokens {
                task_tag: r.get(0)?,
                collab_session_id: r.get(1)?,
                tokens_to_done: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                provider_cost_usd: r.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(MemoryError::from)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::Database;
    use rusqlite::OptionalExtension;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn sample_token_usage() -> NewTokenUsage {
        NewTokenUsage {
            ts: "2026-06-11T00:00:00Z".into(),
            source: "mcp_response".into(),
            harness: "claude".into(),
            model: Some("claude-opus-4-8".into()),
            tool_name: None,
            session_id: Some("sess-1".into()),
            collab_session_id: Some("collab-1".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("issue-80".into()),
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 7,
            estimated: true,
            chars: 480,
            cost_usd: Some(0.0123),
            map_status: None,
            turn_id: None,
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        }
    }

    fn sample_occupancy_sample(ts: &str) -> NewOccupancySample {
        NewOccupancySample {
            ts: ts.into(),
            harness: "claude".into(),
            session_id: Some("sess-1".into()),
            workspace_root: Some("/repo".into()),
            hook_event: Some("precompact".into()),
            input_tokens: 150_000,
            cache_read_input_tokens: 90_000,
            context_window: 200_000,
            occupancy_pct: Some(1.2),
        }
    }

    fn sample_task_outcome(task_tag: &str, started_at: &str) -> TaskOutcome {
        TaskOutcome {
            task_tag: task_tag.into(),
            collab_session_id: Some("collab-1".into()),
            started_at: Some(started_at.into()),
            done_at: None,
            outcome: None,
            review_rounds: 0,
            fix_commits: 0,
            handoffs: 0,
            pr_url: None,
        }
    }

    #[test]
    fn new_token_usage_from_llm_maps_fields() {
        use ironrace_rerank::{LlmResponse, Usage};
        let resp = LlmResponse {
            text: "hello".to_string(),
            usage: Usage {
                input_tokens: 120,
                output_tokens: 3,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 40,
            },
            cost_usd: Some(0.0012),
            model: "claude-haiku-4-5".to_string(),
            estimated: false,
            prompt_chars: 200,
        };
        let row = new_token_usage_from_llm(
            "pref_extract",
            &resp,
            "2026-06-12T00:00:00+00:00".to_string(),
        );
        assert_eq!(row.source, "pref_extract");
        assert_eq!(row.harness, "claude");
        assert_eq!(row.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(row.input_tokens, 120);
        assert_eq!(row.output_tokens, 3);
        assert_eq!(row.cache_read_input_tokens, 40);
        assert_eq!(row.cache_creation_input_tokens, 1);
        assert!(!row.estimated);
        assert_eq!(row.chars, 205); // prompt_chars 200 + text "hello" 5
        assert_eq!(row.cost_usd, Some(0.0012));
        assert!(
            row.session_id.is_none()
                && row.collab_session_id.is_none()
                && row.collab_phase.is_none()
                && row.task_tag.is_none()
        );
    }

    #[test]
    fn token_usage_round_trip() {
        let db = db();
        let id = db.insert_token_usage(&sample_token_usage()).unwrap();
        assert!(id > 0);

        let by_tag = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-80".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_tag.len(), 1);
        let row = &by_tag[0];
        assert_eq!(row.id, id);
        assert_eq!(row.source, "mcp_response");
        assert_eq!(row.input_tokens, 100);
        assert!(row.estimated);
        assert!((row.cost_usd.unwrap() - 0.0123).abs() < 1e-9);

        let by_collab = db
            .query_token_usage(&TokenUsageQuery {
                collab_session_id: Some("collab-1".into()),
                collab_phase: Some("impl".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_collab.len(), 1);
    }

    #[test]
    fn token_usage_round_trip_preserves_nullable_fields() {
        let db = db();
        let mut row = sample_token_usage();
        row.model = None;
        row.session_id = None;
        row.collab_session_id = None;
        row.collab_phase = None;
        row.task_tag = Some("issue-null".into());
        row.cost_usd = None;

        db.insert_token_usage(&row).unwrap();

        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-null".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].model.is_none());
        assert!(rows[0].session_id.is_none());
        assert!(rows[0].collab_session_id.is_none());
        assert!(rows[0].collab_phase.is_none());
        assert!(rows[0].cost_usd.is_none());
    }

    #[test]
    fn token_usage_query_matches_all_and_is_unlimited_by_default() {
        let db = db();
        // Three distinct rows; vary task_tag so no single filter would match all.
        for (i, ts) in [
            "2026-06-11T00:00:01Z",
            "2026-06-11T00:00:02Z",
            "2026-06-11T00:00:03Z",
        ]
        .into_iter()
        .enumerate()
        {
            let mut r = sample_token_usage();
            r.ts = ts.into();
            r.task_tag = Some(format!("issue-{i}"));
            db.insert_token_usage(&r).unwrap();
        }

        // Default query: all filters None (match-all) and limit None (unlimited,
        // mapped to LIMIT -1). A regression mapping None -> 0 would return zero rows.
        let rows = db.query_token_usage(&TokenUsageQuery::default()).unwrap();
        assert_eq!(
            rows.len(),
            3,
            "empty filter + None limit must return every row"
        );
        // Match-all path still applies the ORDER BY (ts, id).
        assert_eq!(rows[0].ts, "2026-06-11T00:00:01Z");
        assert_eq!(rows[2].ts, "2026-06-11T00:00:03Z");
    }

    #[test]
    fn token_usage_query_filter_actually_discriminates() {
        let db = db();
        let mut keep = sample_token_usage();
        keep.task_tag = Some("keep".into());
        db.insert_token_usage(&keep).unwrap();
        let mut skip = sample_token_usage();
        skip.task_tag = Some("skip".into());
        db.insert_token_usage(&skip).unwrap();

        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("keep".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_tag.as_deref(), Some("keep"));
    }

    #[test]
    fn metrics_accept_every_valid_enum_value() {
        // Positive counterpart to the *_rejects_* tests: a typo dropping a legal
        // value from a CHECK list would otherwise pass unnoticed.
        let db = db();
        for source in ["llm_rerank", "pref_extract", "transcript", "mcp_response"] {
            let mut r = sample_token_usage();
            r.source = source.into();
            assert!(
                db.insert_token_usage(&r).is_ok(),
                "source {source} should be accepted"
            );
        }
        for harness in ["claude", "codex"] {
            let mut r = sample_token_usage();
            r.harness = harness.into();
            assert!(
                db.insert_token_usage(&r).is_ok(),
                "harness {harness} should be accepted"
            );
        }
        for phase in ["planning", "impl", "review", "rework", "other"] {
            let mut r = sample_token_usage();
            r.collab_phase = Some(phase.into());
            assert!(
                db.insert_token_usage(&r).is_ok(),
                "collab_phase {phase} should be accepted"
            );
        }
        for hook in [
            "session-start",
            "session-stop",
            "precompact",
            "user-prompt-submit",
        ] {
            let mut r = sample_occupancy_sample("2026-06-11T00:00:00Z");
            r.hook_event = Some(hook.into());
            assert!(
                db.insert_occupancy_sample(&r).is_ok(),
                "hook_event {hook} should be accepted"
            );
        }
        for outcome in ["merged", "failed", "abandoned"] {
            let mut t = sample_task_outcome(&format!("task-{outcome}"), "2026-06-11T00:00:00Z");
            t.outcome = Some(outcome.into());
            assert!(
                db.upsert_task_outcome(&t).is_ok(),
                "outcome {outcome} should be accepted"
            );
        }
    }

    #[test]
    fn token_usage_query_is_ordered_and_limited() {
        let db = db();
        for ts in [
            "2026-06-11T00:00:03Z",
            "2026-06-11T00:00:01Z",
            "2026-06-11T00:00:02Z",
        ] {
            let mut r = sample_token_usage();
            r.ts = ts.into();
            db.insert_token_usage(&r).unwrap();
        }
        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-80".into()),
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts, "2026-06-11T00:00:01Z");
        assert_eq!(rows[1].ts, "2026-06-11T00:00:02Z");
    }

    #[test]
    fn token_usage_soft_fk_survives_unknown_collab_session() {
        let db = db();
        let mut r = sample_token_usage();
        r.collab_session_id = Some("does-not-exist".into());
        assert!(db.insert_token_usage(&r).is_ok());
    }

    #[test]
    fn token_usage_rejects_bad_enums_and_negatives() {
        let db = db();
        // Migration 013 relaxed harness from IN ('claude','codex') to a registry slug
        // GLOB check. Use "BOGUS" (uppercase) which still fails the slug check.
        let mutations: [fn(&mut NewTokenUsage); 9] = [
            |r| r.source = "bogus".into(),
            |r| r.harness = "BOGUS".into(),
            |r| r.collab_phase = Some("bogus".into()),
            |r| r.input_tokens = -1,
            |r| r.output_tokens = -1,
            |r| r.cache_creation_input_tokens = -1,
            |r| r.cache_read_input_tokens = -1,
            |r| r.chars = -1,
            |r| r.cost_usd = Some(-0.01),
        ];
        for mutate in mutations {
            let mut r = sample_token_usage();
            mutate(&mut r);
            assert!(db.insert_token_usage(&r).is_err());
        }
    }

    #[test]
    fn occupancy_round_trip_allows_over_one() {
        let db = db();
        let id = db
            .insert_occupancy_sample(&sample_occupancy_sample("2026-06-11T00:00:00Z"))
            .unwrap();
        assert!(id > 0);
        let rows = db.occupancy_samples_for_session("sess-1", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].occupancy_pct.unwrap() - 1.2).abs() < 1e-9);
    }

    #[test]
    fn occupancy_round_trip_preserves_nullable_fields() {
        let db = db();
        let mut row = sample_occupancy_sample("2026-06-11T00:00:00Z");
        row.workspace_root = None;
        row.hook_event = None;
        row.occupancy_pct = None;

        db.insert_occupancy_sample(&row).unwrap();

        let rows = db.occupancy_samples_for_session("sess-1", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].workspace_root.is_none());
        assert!(rows[0].hook_event.is_none());
        assert!(rows[0].occupancy_pct.is_none());
    }

    #[test]
    fn occupancy_samples_are_ordered_and_limited() {
        let db = db();
        for ts in [
            "2026-06-11T00:00:03Z",
            "2026-06-11T00:00:01Z",
            "2026-06-11T00:00:02Z",
        ] {
            db.insert_occupancy_sample(&sample_occupancy_sample(ts))
                .unwrap();
        }

        let rows = db.occupancy_samples_for_session("sess-1", 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ts, "2026-06-11T00:00:01Z");
        assert_eq!(rows[1].ts, "2026-06-11T00:00:02Z");
    }

    #[test]
    fn occupancy_rejects_bad_hook_event() {
        let db = db();
        let mut r = sample_occupancy_sample("2026-06-11T00:00:00Z");
        r.hook_event = Some("bogus".into());
        assert!(db.insert_occupancy_sample(&r).is_err());
    }

    #[test]
    fn occupancy_rejects_bad_harness_and_negatives() {
        let db = db();
        // Migration 013 relaxed harness from IN ('claude','codex') to a registry slug
        // GLOB check. Use "BOGUS" (uppercase) which still fails the slug check.
        let mutations: [fn(&mut NewOccupancySample); 5] = [
            |r| r.harness = "BOGUS".into(),
            |r| r.input_tokens = -1,
            |r| r.cache_read_input_tokens = -1,
            |r| r.context_window = 0,
            |r| r.occupancy_pct = Some(-0.01),
        ];
        for mutate in mutations {
            let mut r = sample_occupancy_sample("2026-06-11T00:00:00Z");
            mutate(&mut r);
            assert!(db.insert_occupancy_sample(&r).is_err());
        }
    }

    #[test]
    fn session_summary_upsert_updates() {
        let db = db();
        let mut s = SessionSummary {
            session_id: "sess-1".into(),
            harness: "claude".into(),
            workspace_root: Some("/repo".into()),
            started_at: Some("2026-06-11T00:00:00Z".into()),
            ended_at: None,
            peak_occupancy_pct: Some(0.4),
            total_input_tokens: 10,
            total_output_tokens: 2,
            mcp_chars_served: 100,
            compactions: 0,
        };
        db.upsert_session_summary(&s).unwrap();
        s.ended_at = Some("2026-06-11T01:00:00Z".into());
        s.peak_occupancy_pct = Some(0.8);
        s.total_input_tokens = 50;
        s.compactions = 1;
        db.upsert_session_summary(&s).unwrap();

        let got = db.get_session_summary("sess-1").unwrap().unwrap();
        assert_eq!(got.ended_at.as_deref(), Some("2026-06-11T01:00:00Z"));
        assert_eq!(got.total_input_tokens, 50);
        assert_eq!(got.compactions, 1);
        assert!((got.peak_occupancy_pct.unwrap() - 0.8).abs() < 1e-9);
        assert!(db.get_session_summary("missing").unwrap().is_none());
    }

    #[test]
    fn session_summary_rejects_bad_harness() {
        let db = db();
        // Migration 013 relaxed harness from IN ('claude','codex') to a registry slug
        // GLOB check. Use "BOGUS" (uppercase) which still fails the slug check.
        let s = SessionSummary {
            session_id: "sess-1".into(),
            harness: "BOGUS".into(),
            workspace_root: None,
            started_at: None,
            ended_at: None,
            peak_occupancy_pct: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            mcp_chars_served: 0,
            compactions: 0,
        };
        assert!(db.upsert_session_summary(&s).is_err());
    }

    #[test]
    fn session_summary_rejects_negative_counters() {
        let db = db();
        let mutations: [fn(&mut SessionSummary); 5] = [
            |s| s.peak_occupancy_pct = Some(-0.01),
            |s| s.total_input_tokens = -1,
            |s| s.total_output_tokens = -1,
            |s| s.mcp_chars_served = -1,
            |s| s.compactions = -1,
        ];
        for mutate in mutations {
            let mut s = SessionSummary {
                session_id: "sess-1".into(),
                harness: "claude".into(),
                workspace_root: None,
                started_at: None,
                ended_at: None,
                peak_occupancy_pct: None,
                total_input_tokens: 0,
                total_output_tokens: 0,
                mcp_chars_served: 0,
                compactions: 0,
            };
            mutate(&mut s);
            assert!(db.upsert_session_summary(&s).is_err());
        }
    }

    #[test]
    fn task_outcome_upsert_and_query() {
        let db = db();
        let mut t = sample_task_outcome("issue-80", "2026-06-11T00:00:00Z");
        db.upsert_task_outcome(&t).unwrap();
        t.done_at = Some("2026-06-11T02:00:00Z".into());
        t.outcome = Some("merged".into());
        t.review_rounds = 2;
        t.pr_url = Some("https://example/pr/1".into());
        db.upsert_task_outcome(&t).unwrap();

        let got = db.get_task_outcome("issue-80").unwrap().unwrap();
        assert_eq!(got.outcome.as_deref(), Some("merged"));
        assert_eq!(got.review_rounds, 2);

        let for_collab = db.task_outcomes_for_collab("collab-1").unwrap();
        assert_eq!(for_collab.len(), 1);
        assert!(db.get_task_outcome("missing").unwrap().is_none());
    }

    #[test]
    fn task_outcomes_for_collab_is_ordered_by_started_at_then_id() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("task-b", "2026-06-11T00:00:02Z"))
            .unwrap();
        db.upsert_task_outcome(&sample_task_outcome("task-a", "2026-06-11T00:00:01Z"))
            .unwrap();
        db.upsert_task_outcome(&sample_task_outcome("task-c", "2026-06-11T00:00:01Z"))
            .unwrap();

        let rows = db.task_outcomes_for_collab("collab-1").unwrap();
        let task_tags: Vec<_> = rows.iter().map(|row| row.task_tag.as_str()).collect();
        assert_eq!(task_tags, ["task-a", "task-c", "task-b"]);
    }

    #[test]
    fn task_outcome_rejects_bad_outcome() {
        let db = db();
        let mut t = sample_task_outcome("issue-80", "2026-06-11T00:00:00Z");
        t.outcome = Some("bogus".into());
        assert!(db.upsert_task_outcome(&t).is_err());
    }

    #[test]
    fn task_outcome_rejects_negative_counters() {
        let db = db();
        let mutations: [fn(&mut TaskOutcome); 3] = [
            |t| t.review_rounds = -1,
            |t| t.fix_commits = -1,
            |t| t.handoffs = -1,
        ];
        for mutate in mutations {
            let mut t = sample_task_outcome("issue-80", "2026-06-11T00:00:00Z");
            mutate(&mut t);
            assert!(db.upsert_task_outcome(&t).is_err());
        }
    }

    #[test]
    fn real_db_migrates_cleanly() {
        let Some(src) = real_db_path() else {
            eprintln!("skip real_db_migrates_cleanly: no readable real DB");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("copy.sqlite3");
        if std::fs::copy(&src, &dst).is_err() {
            eprintln!("skip real_db_migrates_cleanly: copy failed");
            return;
        }
        let db = Database::open(&dst).unwrap();
        db.migrate().unwrap();
        let exists: bool = db
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='token_usage'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();
        assert!(exists);
    }

    fn real_db_path() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("IRONMEM_DB_PATH") {
            let pb = std::path::PathBuf::from(p);
            if pb.is_file() {
                return Some(pb);
            }
        }
        let pb = dirs::home_dir()?
            .join(".ironrace-memory")
            .join("memory.sqlite3");
        pb.is_file().then_some(pb)
    }

    #[test]
    fn increment_task_review_rounds_is_monotonic_and_preserves_row() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("issue-83", "2026-06-12T00:00:00Z"))
            .unwrap();
        db.increment_task_review_rounds("issue-83").unwrap();
        db.increment_task_review_rounds("issue-83").unwrap();
        let got = db.get_task_outcome("issue-83").unwrap().unwrap();
        assert_eq!(got.review_rounds, 2);
        assert_eq!(got.fix_commits, 0); // untouched
        assert_eq!(got.started_at.as_deref(), Some("2026-06-12T00:00:00Z")); // untouched
    }

    #[test]
    fn increment_task_review_rounds_missing_tag_is_noop_ok() {
        let db = db();
        assert!(db.increment_task_review_rounds("missing").is_ok());
    }

    #[test]
    fn increment_task_handoffs_is_monotonic_and_preserves_row() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("issue-93", "2026-06-15T00:00:00Z"))
            .unwrap();
        db.increment_task_handoffs("issue-93").unwrap();
        db.increment_task_handoffs("issue-93").unwrap();
        let got = db.get_task_outcome("issue-93").unwrap().unwrap();
        assert_eq!(got.handoffs, 2);
        assert_eq!(got.review_rounds, 0); // untouched
        assert_eq!(got.fix_commits, 0); // untouched
        assert_eq!(got.started_at.as_deref(), Some("2026-06-15T00:00:00Z")); // untouched
    }

    #[test]
    fn increment_task_handoffs_missing_tag_is_noop_ok_and_creates_no_row() {
        let db = db();
        // Confirm no row exists first
        assert!(db.get_task_outcome("missing-handoff").unwrap().is_none());
        // Increment on absent tag -> Ok, no row created
        assert!(db.increment_task_handoffs("missing-handoff").is_ok());
        // Still no row
        assert!(db.get_task_outcome("missing-handoff").unwrap().is_none());
    }

    #[test]
    fn mark_task_outcome_done_partial_update_preserves_counters_and_nones() {
        let db = db();
        let mut t = sample_task_outcome("issue-83", "2026-06-12T00:00:00Z");
        t.review_rounds = 3;
        db.upsert_task_outcome(&t).unwrap();

        // First call: done_at + pr_url, no outcome (CodingComplete semantics).
        assert_eq!(
            db.mark_task_outcome_done(
                "issue-83",
                Some("2026-06-12T01:00:00Z"),
                None,
                Some("https://example/pr/5"),
            )
            .unwrap(),
            TaskOutcomeAttestation::Recorded
        );
        let got = db.get_task_outcome("issue-83").unwrap().unwrap();
        assert_eq!(got.done_at.as_deref(), Some("2026-06-12T01:00:00Z"));
        assert!(got.outcome.is_none());
        assert_eq!(got.pr_url.as_deref(), Some("https://example/pr/5"));
        assert_eq!(got.review_rounds, 3); // counters preserved

        // Second call: outcome only (collab_end attestation); earlier fields kept.
        assert_eq!(
            db.mark_task_outcome_done("issue-83", None, Some("merged"), None)
                .unwrap(),
            TaskOutcomeAttestation::Recorded
        );
        let got = db.get_task_outcome("issue-83").unwrap().unwrap();
        assert_eq!(got.outcome.as_deref(), Some("merged"));
        assert_eq!(got.done_at.as_deref(), Some("2026-06-12T01:00:00Z")); // not clobbered
        assert_eq!(got.pr_url.as_deref(), Some("https://example/pr/5")); // not clobbered
    }

    #[test]
    fn mark_task_outcome_done_rejects_bad_outcome_enum() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("issue-83", "2026-06-12T00:00:00Z"))
            .unwrap();
        assert!(db
            .mark_task_outcome_done("issue-83", None, Some("bogus"), None)
            .is_err());
    }

    /// A tag with no row is still `Ok` — attestation is best-effort and must
    /// never fail the turn that triggered it — but it is now a *distinguishable*
    /// `Ok`. `collab_end`'s abandon arm is the caller that needs the
    /// difference: it is the only writer whose session no later surface can
    /// re-attest, because the seal refuses every mutating call afterwards, so
    /// a silently-missed attestation stands forever.
    #[test]
    fn mark_task_outcome_done_reports_a_missing_row_without_failing() {
        let db = db();
        assert_eq!(
            db.mark_task_outcome_done("never-created", None, Some("abandoned"), None)
                .unwrap(),
            TaskOutcomeAttestation::NoRow,
            "a 0-row UPDATE must be reported, not folded into a successful attestation"
        );
    }

    #[test]
    fn clear_failed_task_outcome_nulls_outcome_and_done_at() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("issue-83", "2026-06-12T00:00:00Z"))
            .unwrap();
        assert_eq!(
            db.mark_task_outcome_done(
                "issue-83",
                Some("2026-06-12T01:00:00Z"),
                Some("failed"),
                None,
            )
            .unwrap(),
            TaskOutcomeAttestation::Recorded
        );
        let got = db.get_task_outcome("issue-83").unwrap().unwrap();
        assert_eq!(got.outcome.as_deref(), Some("failed"));
        assert!(got.done_at.is_some());

        db.clear_failed_task_outcome("issue-83").unwrap();

        let got = db.get_task_outcome("issue-83").unwrap().unwrap();
        assert_eq!(got.outcome, None, "outcome must be nulled");
        assert_eq!(got.done_at, None, "done_at must be nulled");
    }

    #[test]
    fn clear_failed_task_outcome_is_scoped_to_failed_only() {
        let db = db();
        db.upsert_task_outcome(&sample_task_outcome("issue-83", "2026-06-12T00:00:00Z"))
            .unwrap();
        assert_eq!(
            db.mark_task_outcome_done(
                "issue-83",
                Some("2026-06-12T01:00:00Z"),
                Some("merged"),
                None,
            )
            .unwrap(),
            TaskOutcomeAttestation::Recorded
        );

        db.clear_failed_task_outcome("issue-83").unwrap();

        let got = db.get_task_outcome("issue-83").unwrap().unwrap();
        assert_eq!(
            got.outcome.as_deref(),
            Some("merged"),
            "a non-'failed' outcome must not be clobbered by clear_failed_task_outcome"
        );
        assert!(got.done_at.is_some(), "done_at must be preserved too");
    }

    #[test]
    fn clear_failed_task_outcome_missing_tag_is_noop_ok() {
        let db = db();
        assert!(db.get_task_outcome("missing-clear").unwrap().is_none());
        assert!(db.clear_failed_task_outcome("missing-clear").is_ok());
        assert!(db.get_task_outcome("missing-clear").unwrap().is_none());
    }

    // ---- §10 report aggregates ----

    #[allow(clippy::too_many_arguments)]
    fn tok(
        task_key_collab: Option<&str>,
        phase: &str,
        model: &str,
        harness: &str,
        inp: i64,
        out: i64,
        cc: i64,
        cr: i64,
        estimated: bool,
        cost: Option<f64>,
        ts: &str,
    ) -> NewTokenUsage {
        NewTokenUsage {
            ts: ts.into(),
            source: "llm_rerank".into(),
            harness: harness.into(),
            model: Some(model.into()),
            tool_name: None,
            session_id: None,
            collab_session_id: task_key_collab.map(|s| s.into()),
            collab_phase: Some(phase.into()),
            task_tag: None,
            input_tokens: inp,
            output_tokens: out,
            cache_creation_input_tokens: cc,
            cache_read_input_tokens: cr,
            estimated,
            chars: 0,
            cost_usd: cost,
            map_status: None,
            turn_id: None,
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        }
    }

    #[test]
    fn report_tokens_by_task_phase_rolls_up_to_spec_10_1() {
        let db = db();
        // Two measured rows, same (task,phase) different model+harness → two groups.
        db.insert_token_usage(&tok(
            Some("S"),
            "impl",
            "claude-opus-4-8",
            "claude",
            100,
            10,
            0,
            0,
            false,
            Some(0.5),
            "2026-06-01T00:00:00Z",
        ))
        .unwrap();
        db.insert_token_usage(&tok(
            Some("S"),
            "impl",
            "claude-sonnet-4-6",
            "codex",
            200,
            0,
            0,
            0,
            false,
            None,
            "2026-06-01T00:00:01Z",
        ))
        .unwrap();
        // One ESTIMATED row must be excluded (§10.1 WHERE estimated=0).
        db.insert_token_usage(&tok(
            Some("S"),
            "impl",
            "claude-opus-4-8",
            "claude",
            999,
            0,
            0,
            0,
            true,
            None,
            "2026-06-01T00:00:02Z",
        ))
        .unwrap();

        let rows = db.report_tokens_by_task_phase(None, None).unwrap();
        assert_eq!(
            rows.len(),
            2,
            "two model/harness groups, estimated excluded"
        );

        // DRIFT GUARD: literal §10.1 (grouped by task_key, collab_phase) must equal the Rust roll-up.
        let literal: Vec<(String, Option<String>, i64)> = db
            .conn
            .prepare(
                "SELECT COALESCE(collab_session_id, task_tag) AS task_key, collab_phase,
                        SUM(input_tokens + output_tokens + cache_creation_input_tokens + cache_read_input_tokens) AS tokens
                 FROM token_usage WHERE estimated = 0 AND COALESCE(collab_session_id, task_tag) IS NOT NULL
                 GROUP BY task_key, collab_phase ORDER BY task_key, collab_phase",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        // Roll the method's finer grain up to (task_key, phase).
        let mut rolled: std::collections::BTreeMap<(String, Option<String>), i64> =
            Default::default();
        for r in &rows {
            *rolled
                .entry((r.task_key.clone(), r.collab_phase.clone()))
                .or_default() += r.input_tokens
                + r.output_tokens
                + r.cache_creation_input_tokens
                + r.cache_read_input_tokens;
        }
        let rolled: Vec<_> = rolled.into_iter().map(|((k, p), t)| (k, p, t)).collect();
        assert_eq!(
            literal, rolled,
            "§10.1-compatible roll-up must equal literal §10.1"
        );
    }

    #[test]
    fn report_tokens_provider_cost_preserves_null() {
        let db = db();
        db.insert_token_usage(&tok(
            Some("S"),
            "impl",
            "claude-opus-4-8",
            "claude",
            1,
            0,
            0,
            0,
            false,
            None,
            "2026-06-01T00:00:00Z",
        ))
        .unwrap();
        let rows = db.report_tokens_by_task_phase(None, None).unwrap();
        assert_eq!(
            rows[0].provider_cost_usd, None,
            "SUM of all-NULL cost stays NULL, not 0.0"
        );
    }

    #[test]
    fn report_filters_by_task_and_since() {
        let db = db();
        db.insert_token_usage(&tok(
            Some("KEEP"),
            "impl",
            "claude-opus-4-8",
            "claude",
            1,
            0,
            0,
            0,
            false,
            None,
            "2026-06-05T00:00:00Z",
        ))
        .unwrap();
        db.insert_token_usage(&tok(
            Some("DROP"),
            "impl",
            "claude-opus-4-8",
            "claude",
            1,
            0,
            0,
            0,
            false,
            None,
            "2026-06-01T00:00:00Z",
        ))
        .unwrap();
        assert_eq!(
            db.report_tokens_by_task_phase(Some("KEEP"), None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.report_tokens_by_task_phase(None, Some("2026-06-03"))
                .unwrap()
                .len(),
            1,
            "since filters ts"
        );
    }

    #[test]
    fn report_headline_excludes_non_merged_and_non_completions_include_them() {
        let db = db();
        // merged task with a measured row
        let mut merged = sample_task_outcome("issue-m", "2026-06-01T00:00:00Z");
        merged.collab_session_id = Some("S-m".into());
        merged.outcome = Some("merged".into());
        db.upsert_task_outcome(&merged).unwrap();
        db.insert_token_usage(&tok(
            Some("S-m"),
            "impl",
            "claude-opus-4-8",
            "claude",
            1000,
            0,
            0,
            0,
            false,
            None,
            "2026-06-01T00:00:00Z",
        ))
        .unwrap();
        // failed task with a measured row — MUST NOT appear in the headline.
        let mut failed = sample_task_outcome("issue-f", "2026-06-02T00:00:00Z");
        failed.collab_session_id = Some("S-f".into());
        failed.outcome = Some("failed".into());
        db.upsert_task_outcome(&failed).unwrap();
        db.insert_token_usage(&tok(
            Some("S-f"),
            "impl",
            "claude-opus-4-8",
            "claude",
            5000,
            0,
            0,
            0,
            false,
            None,
            "2026-06-02T00:00:00Z",
        ))
        .unwrap();

        let head = db.report_headline(None, None).unwrap();
        assert_eq!(head.len(), 1, "only merged tasks in headline");
        assert_eq!(head[0].task_tag, "issue-m");
        assert_eq!(head[0].tokens_to_done, 1000);

        let non = db.report_non_completions(None, None).unwrap();
        assert_eq!(non.len(), 1);
        assert_eq!(non[0].task_tag, "issue-f");
        assert_eq!(non[0].tokens_to_done, 5000);
    }

    // ---- Phase 5 / issue #94: exploration-token attribution ----

    #[test]
    fn test_record_exploration_tokens_map_hit() {
        let db = db();
        db.record_exploration_tokens(
            "2026-06-15T00:00:00Z",
            "claude",
            100,
            50,
            Some(MapStatus::Hit),
            Some("t1"),
            Some("core"),
        )
        .unwrap();

        let rows = db.query_token_usage(&TokenUsageQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "mcp_response");
        assert!(!rows[0].estimated);
        assert_eq!(rows[0].map_status, Some(MapStatus::Hit));
        assert_eq!(rows[0].turn_id.as_deref(), Some("t1"));
        assert_eq!(rows[0].area.as_deref(), Some("core"));
    }

    #[test]
    fn test_record_exploration_tokens_map_miss() {
        let db = db();
        db.record_exploration_tokens(
            "2026-06-15T00:00:01Z",
            "codex",
            200,
            80,
            Some(MapStatus::Miss),
            Some("t2"),
            Some("auth"),
        )
        .unwrap();

        let rows = db.query_token_usage(&TokenUsageQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "mcp_response");
        assert!(!rows[0].estimated);
        assert_eq!(rows[0].map_status, Some(MapStatus::Miss));
        assert_eq!(rows[0].turn_id.as_deref(), Some("t2"));
        assert_eq!(rows[0].area.as_deref(), Some("auth"));
    }

    #[test]
    fn test_report_exploration_delta_hit_rate() {
        let db = db();

        // t1 = map_hit, 300 total tokens
        db.record_exploration_tokens(
            "2026-06-15T00:00:01Z",
            "claude",
            200,
            100,
            Some(MapStatus::Hit),
            Some("t1"),
            Some("core"),
        )
        .unwrap();

        // t2 = map_hit, 150 total tokens
        db.record_exploration_tokens(
            "2026-06-15T00:00:02Z",
            "claude",
            100,
            50,
            Some(MapStatus::Hit),
            Some("t2"),
            Some("auth"),
        )
        .unwrap();

        // t3 = map_miss, 400 total tokens
        db.record_exploration_tokens(
            "2026-06-15T00:00:03Z",
            "claude",
            300,
            100,
            Some(MapStatus::Miss),
            Some("t3"),
            Some("db"),
        )
        .unwrap();

        let report = db.report_exploration_delta().unwrap();
        assert_eq!(report.total_turns, 3);
        assert_eq!(report.map_hit_turns, 2);
        assert_eq!(report.map_miss_turns, 1);
        // hit_rate = 2/3
        assert!((report.hit_rate - 2.0 / 3.0).abs() < 1e-9);
        // mean_tokens_map_hit = (300 + 150) / 2 = 225
        assert!((report.mean_tokens_map_hit - 225.0).abs() < 1e-9);
        // mean_tokens_map_miss = 400 / 1 = 400
        assert!((report.mean_tokens_map_miss - 400.0).abs() < 1e-9);
    }

    /// A turn that emits BOTH a hit and a miss counts as exactly ONE turn with
    /// the conservative `map_miss` verdict (a hit + no miss is the only hit).
    /// Rows with NULL turn_id are excluded entirely (unattributable).
    #[test]
    fn test_report_exploration_delta_dedups_turn_and_excludes_null() {
        let db = db();

        // Turn "t1" emits a hit (100) then a miss (200) → ONE miss turn, 300 tok.
        db.record_exploration_tokens(
            "2026-06-15T00:00:01Z",
            "claude",
            60,
            40,
            Some(MapStatus::Hit),
            Some("t1"),
            Some("core"),
        )
        .unwrap();
        db.record_exploration_tokens(
            "2026-06-15T00:00:02Z",
            "claude",
            120,
            80,
            Some(MapStatus::Miss),
            Some("t1"),
            Some("core"),
        )
        .unwrap();

        // Turn "t2" emits only a hit → ONE hit turn, 150 tok.
        db.record_exploration_tokens(
            "2026-06-15T00:00:03Z",
            "claude",
            100,
            50,
            Some(MapStatus::Hit),
            Some("t2"),
            Some("auth"),
        )
        .unwrap();

        // NULL turn_id row must be ignored (cannot attribute to a turn).
        db.record_exploration_tokens(
            "2026-06-15T00:00:04Z",
            "claude",
            500,
            500,
            Some(MapStatus::Hit),
            None,
            Some("db"),
        )
        .unwrap();

        let report = db.report_exploration_delta().unwrap();
        // 2 turns total: t1 (miss), t2 (hit). NULL row excluded.
        assert_eq!(report.total_turns, 2);
        assert_eq!(report.map_hit_turns, 1);
        assert_eq!(report.map_miss_turns, 1);
        assert!((report.hit_rate - 0.5).abs() < 1e-9);
        // t2 is the only hit turn → 150 tokens.
        assert!((report.mean_tokens_map_hit - 150.0).abs() < 1e-9);
        // t1 is the only miss turn → 100 + 200 = 300 tokens.
        assert!((report.mean_tokens_map_miss - 300.0).abs() < 1e-9);
    }

    // ---- Migration 011: map_status / turn_id / area columns on token_usage ----

    #[test]
    fn test_token_usage_map_status_round_trip() {
        let db = db();
        let mut r = sample_token_usage();
        r.map_status = Some(MapStatus::Hit);
        r.turn_id = Some("turn-42".into());
        r.area = Some("src/auth".into());
        let id = db.insert_token_usage(&r).unwrap();

        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-80".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].map_status, Some(MapStatus::Hit));
        assert_eq!(rows[0].turn_id.as_deref(), Some("turn-42"));
        assert_eq!(rows[0].area.as_deref(), Some("src/auth"));
    }

    #[test]
    fn test_token_usage_map_status_null_default() {
        let db = db();
        // sample_token_usage() uses default NewTokenUsage which has map_status=None
        db.insert_token_usage(&sample_token_usage()).unwrap();

        let rows = db
            .query_token_usage(&TokenUsageQuery {
                task_tag: Some("issue-80".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].map_status.is_none(),
            "map_status must default to None"
        );
        assert!(rows[0].turn_id.is_none(), "turn_id must default to None");
        assert!(rows[0].area.is_none(), "area must default to None");
    }

    #[test]
    fn test_token_usage_map_status_invalid_rejected() {
        let db = db();
        // Insert a valid row first to confirm the table is writable.
        db.insert_token_usage(&sample_token_usage()).unwrap();
        // Now try a raw SQL insert with an invalid map_status value.
        let result = db.conn.execute(
            "INSERT INTO token_usage (ts, source, harness, input_tokens, output_tokens,
             cache_creation_input_tokens, cache_read_input_tokens, estimated, chars, map_status)
             VALUES ('2026-06-15T00:00:00Z','mcp_response','claude',1,1,0,0,0,10,'invalid')",
            [],
        );
        assert!(
            result.is_err(),
            "CHECK constraint on map_status must reject 'invalid'"
        );
    }

    #[test]
    fn report_split_separates_measured_and_estimated() {
        let db = db();
        db.insert_token_usage(&tok(
            Some("S"),
            "impl",
            "claude-opus-4-8",
            "claude",
            100,
            0,
            0,
            0,
            false,
            None,
            "2026-06-01T00:00:00Z",
        ))
        .unwrap();
        db.insert_token_usage(&tok(
            Some("S"),
            "impl",
            "claude-opus-4-8",
            "claude",
            40,
            0,
            0,
            0,
            true,
            None,
            "2026-06-01T00:00:01Z",
        ))
        .unwrap();
        let split = db.report_measured_estimated_split(None, None).unwrap();
        let measured: i64 = split
            .iter()
            .filter(|s| !s.estimated)
            .map(|s| s.tokens)
            .sum();
        let estimated: i64 = split.iter().filter(|s| s.estimated).map(|s| s.tokens).sum();
        assert_eq!((measured, estimated), (100, 40));
    }

    // ── Tasks 4 + 5: upsert_transcript_token_usage idempotency ────────────────

    fn transcript_row(turn_id: &str, inp: i64, out: i64, cc: i64, cr: i64) -> NewTokenUsage {
        NewTokenUsage {
            ts: "2026-06-19T00:00:00Z".into(),
            source: "transcript".into(),
            harness: "claude".into(),
            model: Some("claude-sonnet-4-6".into()),
            tool_name: None,
            session_id: Some("sess-t1".into()),
            collab_session_id: Some("collab-t1".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("collab-t1".into()),
            input_tokens: inp,
            output_tokens: out,
            cache_creation_input_tokens: cc,
            cache_read_input_tokens: cr,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: Some(turn_id.to_string()),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        }
    }

    #[test]
    fn upsert_transcript_twice_yields_one_row_with_latest_usage() {
        let db = db();
        let first = transcript_row("transcript:sess-t1:msg-1", 100, 20, 5, 10);
        db.upsert_transcript_token_usage(&first).unwrap();

        // Second upsert with different (updated) token values.
        let second = transcript_row("transcript:sess-t1:msg-1", 999, 888, 7, 6);
        db.upsert_transcript_token_usage(&second).unwrap();

        // Exactly one transcript row.
        let rows = db
            .query_token_usage(&TokenUsageQuery {
                ..Default::default()
            })
            .unwrap();
        let transcript_rows: Vec<_> = rows.iter().filter(|r| r.source == "transcript").collect();
        assert_eq!(
            transcript_rows.len(),
            1,
            "idempotent: exactly one row after two upserts"
        );
        assert_eq!(transcript_rows[0].input_tokens, 999, "latest usage stored");
        assert_eq!(transcript_rows[0].output_tokens, 888);
        assert_eq!(transcript_rows[0].cache_creation_input_tokens, 7);
        assert_eq!(transcript_rows[0].cache_read_input_tokens, 6);
        assert!(!transcript_rows[0].estimated, "estimated must be false");
        assert!(transcript_rows[0].cost_usd.is_none());
    }

    #[test]
    fn upsert_transcript_does_not_touch_mcp_response_row_sharing_nominal_turn_id() {
        let db = db();
        // Insert an mcp_response row with the same turn_id string.
        let mcp_row = NewTokenUsage {
            ts: "2026-06-19T00:00:00Z".into(),
            source: "mcp_response".into(),
            harness: "claude".into(),
            model: None,
            tool_name: None,
            session_id: None,
            collab_session_id: None,
            collab_phase: None,
            task_tag: None,
            input_tokens: 0,
            output_tokens: 42,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: true,
            chars: 168,
            cost_usd: None,
            map_status: None,
            turn_id: Some("transcript:sess-t1:msg-1".to_string()),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        };
        db.insert_token_usage(&mcp_row).unwrap();

        // Now upsert a transcript row with the same turn_id.
        let tx_row = transcript_row("transcript:sess-t1:msg-1", 100, 20, 0, 0);
        db.upsert_transcript_token_usage(&tx_row).unwrap();

        // Two total rows: one mcp_response (untouched) + one transcript (new).
        let all = db.query_token_usage(&TokenUsageQuery::default()).unwrap();
        assert_eq!(all.len(), 2, "mcp_response row must not be clobbered");

        let mcp = all.iter().find(|r| r.source == "mcp_response").unwrap();
        assert_eq!(mcp.output_tokens, 42, "mcp_response row unchanged");
        assert!(mcp.estimated, "mcp_response estimated flag unchanged");

        let tx = all.iter().find(|r| r.source == "transcript").unwrap();
        assert_eq!(tx.input_tokens, 100);
        assert!(!tx.estimated);
    }

    // ── Task 9: report regression — transcript rows flow into §10.4 headline ──

    #[test]
    fn transcript_rows_flow_into_headline_tokens_to_done() {
        // METRICS_SPEC §10.4: the headline JOIN is on
        //   `u.task_tag = t.task_tag OR u.collab_session_id = t.collab_session_id`
        // and uses `estimated = 0`. Transcript rows (source='transcript', estimated=false)
        // must appear in `tokens_to_done` when their collab_session_id matches the
        // task_outcomes row.
        let db = db();

        // Seed a task_outcomes row.
        db.upsert_task_outcome(&TaskOutcome {
            task_tag: "tx-task".into(),
            collab_session_id: Some("tx-collab".into()),
            started_at: Some("2026-06-19T00:00:00Z".into()),
            done_at: Some("2026-06-19T01:00:00Z".into()),
            outcome: Some("merged".into()),
            review_rounds: 1,
            fix_commits: 2,
            handoffs: 0,
            pr_url: None,
        })
        .unwrap();

        // Claude transcript row (collab_session_id match).
        db.insert_token_usage(&NewTokenUsage {
            ts: "2026-06-19T00:30:00Z".into(),
            source: "transcript".into(),
            harness: "claude".into(),
            model: Some("claude-sonnet-4-6".into()),
            tool_name: None,
            session_id: Some("sess-1".into()),
            collab_session_id: Some("tx-collab".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("tx-collab".into()),
            input_tokens: 300,
            output_tokens: 60,
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 50,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: Some("transcript:sess-1:msg-1".into()),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        })
        .unwrap();

        // Codex transcript row (cached subtracted from input, collab_session_id match).
        db.insert_token_usage(&NewTokenUsage {
            ts: "2026-06-19T00:45:00Z".into(),
            source: "transcript".into(),
            harness: "codex".into(),
            model: None,
            tool_name: None,
            session_id: Some("codex-sess-1".into()),
            collab_session_id: Some("tx-collab".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("tx-collab".into()),
            input_tokens: 900, // 1500 − 600
            output_tokens: 300,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 600,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: Some("transcript:codex-sess-1:codex-final".into()),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        })
        .unwrap();

        let headline = db.report_headline(Some("tx-task"), None).unwrap();
        assert_eq!(headline.len(), 1, "one merged task");
        let h = &headline[0];
        // Expected: (300+60+10+50) + (900+300+0+600) = 420 + 1800 = 2220
        assert_eq!(
            h.tokens_to_done, 2220,
            "transcript rows (claude + codex) must sum into tokens_to_done"
        );
        assert_eq!(h.task_tag, "tx-task");
        assert_eq!(h.collab_session_id.as_deref(), Some("tx-collab"));
    }

    #[test]
    fn estimated_transcript_rows_are_excluded_from_headline() {
        // `source='transcript'` rows with `estimated=true` must NOT appear in the
        // §10.4 headline (estimated=0 filter). This is a guardrail: the production
        // path always writes estimated=false, but a stale row must not pollute.
        let db = db();
        db.upsert_task_outcome(&TaskOutcome {
            task_tag: "est-task".into(),
            collab_session_id: Some("est-collab".into()),
            started_at: Some("2026-06-19T00:00:00Z".into()),
            done_at: Some("2026-06-19T01:00:00Z".into()),
            outcome: Some("merged".into()),
            review_rounds: 0,
            fix_commits: 0,
            handoffs: 0,
            pr_url: None,
        })
        .unwrap();
        db.insert_token_usage(&NewTokenUsage {
            ts: "2026-06-19T00:30:00Z".into(),
            source: "transcript".into(),
            harness: "claude".into(),
            model: None,
            tool_name: None,
            session_id: None,
            collab_session_id: Some("est-collab".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("est-collab".into()),
            input_tokens: 99999,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: true, // <-- must be excluded from §10.4
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: Some("transcript:x:y".into()),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        })
        .unwrap();
        let headline = db.report_headline(Some("est-task"), None).unwrap();
        // Zero rows because the only transcript row is estimated=true.
        assert!(
            headline.is_empty(),
            "estimated transcript rows must not appear in §10.4 headline"
        );
    }

    #[test]
    fn upsert_transcript_codex_row_is_idempotent() {
        let db = db();
        let codex_row = NewTokenUsage {
            ts: "2026-06-19T00:00:00Z".into(),
            source: "transcript".into(),
            harness: "codex".into(),
            model: None,
            tool_name: None,
            session_id: Some("codex-sess-1".into()),
            collab_session_id: Some("collab-t1".into()),
            collab_phase: Some("impl".into()),
            task_tag: Some("collab-t1".into()),
            input_tokens: 900,
            output_tokens: 300,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 600,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: Some("transcript:codex-sess-1:codex-final".to_string()),
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        };
        db.upsert_transcript_token_usage(&codex_row).unwrap();
        db.upsert_transcript_token_usage(&codex_row).unwrap();

        let rows = db.query_token_usage(&TokenUsageQuery::default()).unwrap();
        let codex_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.harness == "codex" && r.source == "transcript")
            .collect();
        assert_eq!(
            codex_rows.len(),
            1,
            "codex transcript row must be idempotent"
        );
        assert_eq!(codex_rows[0].input_tokens, 900);
        assert_eq!(codex_rows[0].cache_read_input_tokens, 600);
    }

    // ── M3: BEGIN IMMEDIATE serializes overlapping cross-connection writers ───

    #[test]
    fn upsert_transcript_is_serialized_across_concurrent_connections() {
        // The `stop` and `precompact` hooks run in SEPARATE processes, so the
        // SELECT/INSERT dedup window must serialize across connections — that is
        // the reason `upsert_transcript_token_usage_many` uses `BEGIN IMMEDIATE`
        // rather than the crate's DEFERRED `with_transaction`. The in-memory
        // `db()` helper gives each handle its OWN database, so it cannot exercise
        // this; use a single file-backed DB opened from two threads.
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.sqlite3");
        // First open creates the file and runs migrations.
        Database::open(&path).unwrap().migrate().unwrap();

        let row = transcript_row("transcript:race:msg-1", 100, 20, 0, 0);
        // Barrier maximizes the overlap: both threads reach the upsert together,
        // so without IMMEDIATE both could SELECT "no row" before either inserts.
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                let row = row.clone();
                std::thread::spawn(move || {
                    let db = Database::open(&path).unwrap();
                    barrier.wait();
                    db.upsert_transcript_token_usage(&row)
                })
            })
            .collect();
        for h in handles {
            // Both writers must succeed (the loser blocks on busy_timeout, then
            // observes the committed row and UPDATEs it) — neither errors.
            h.join().unwrap().unwrap();
        }

        let db = Database::open(&path).unwrap();
        let rows = db.query_token_usage(&TokenUsageQuery::default()).unwrap();
        let tx_rows: Vec<_> = rows.iter().filter(|r| r.source == "transcript").collect();
        assert_eq!(
            tx_rows.len(),
            1,
            "BEGIN IMMEDIATE must serialize concurrent upserts of one turn_id to a single row"
        );
        assert_eq!(tx_rows[0].input_tokens, 100);
    }

    // ---- issue #145: repeated-context indicators for the value summary ----

    /// `report_mcp_response_sizing` aggregates ALL `source='mcp_response'` rows
    /// (not just map-tagged exploration turns): row count + total/mean of the
    /// response-size token proxy (`output_tokens`).
    #[test]
    fn report_mcp_response_sizing_aggregates_rows() {
        let db = db();
        // Two mcp_response rows (record_exploration_tokens writes source='mcp_response').
        db.record_exploration_tokens(
            "2026-06-21T00:00:01Z",
            "claude",
            10,
            50,
            Some(MapStatus::Hit),
            Some("t1"),
            Some("core"),
        )
        .unwrap();
        db.record_exploration_tokens(
            "2026-06-21T00:00:02Z",
            "claude",
            10,
            30,
            Some(MapStatus::Miss),
            Some("t2"),
            Some("auth"),
        )
        .unwrap();

        let sizing = db.report_mcp_response_sizing().unwrap();
        assert_eq!(sizing.row_count, 2);
        assert_eq!(sizing.total_output_tokens, 80);
        assert!((sizing.mean_output_tokens - 40.0).abs() < 1e-9);
    }

    #[test]
    fn report_mcp_response_sizing_groups_top_tools_by_collab_session() {
        let db = db();
        let mut collab_status = sample_token_usage();
        collab_status.collab_session_id = Some("collab-a".into());
        collab_status.tool_name = Some("collab_status".into());
        collab_status.output_tokens = 100;
        collab_status.chars = 400;
        db.insert_token_usage(&collab_status).unwrap();

        let mut get_drawer = sample_token_usage();
        get_drawer.collab_session_id = Some("collab-a".into());
        get_drawer.tool_name = Some("get_drawer".into());
        get_drawer.output_tokens = 25;
        get_drawer.chars = 100;
        db.insert_token_usage(&get_drawer).unwrap();

        let sizing = db.report_mcp_response_sizing().unwrap();
        assert_eq!(sizing.row_count, 2);
        assert_eq!(sizing.top_tools.len(), 2);
        assert_eq!(
            sizing.top_tools[0].collab_session_id.as_deref(),
            Some("collab-a")
        );
        assert_eq!(
            sizing.top_tools[0].tool_name.as_deref(),
            Some("collab_status")
        );
        assert_eq!(sizing.top_tools[0].total_chars, 400);
        assert_eq!(sizing.top_tools[0].total_output_tokens, 100);
    }

    #[test]
    fn report_mcp_response_sizing_empty_is_zero() {
        let db = db();
        let sizing = db.report_mcp_response_sizing().unwrap();
        assert_eq!(sizing.row_count, 0);
        assert_eq!(sizing.total_output_tokens, 0);
        assert_eq!(sizing.mean_output_tokens, 0.0);
        assert!(sizing.top_tools.is_empty());
    }

    /// `report_transcript_coverage` counts distinct transcript-covered turns and
    /// sums their `input + output` tokens over `source='transcript'` rows.
    #[test]
    fn report_transcript_coverage_counts_turns() {
        let db = db();
        db.upsert_transcript_token_usage(&transcript_row("turn-1", 100, 20, 0, 0))
            .unwrap();
        db.upsert_transcript_token_usage(&transcript_row("turn-2", 200, 30, 0, 0))
            .unwrap();

        let cov = db.report_transcript_coverage().unwrap();
        assert_eq!(cov.turn_count, 2);
        assert_eq!(cov.total_tokens, 350);
    }

    #[test]
    fn report_transcript_coverage_empty_is_zero() {
        let db = db();
        let cov = db.report_transcript_coverage().unwrap();
        assert_eq!(cov.turn_count, 0);
        assert_eq!(cov.total_tokens, 0);
    }
}
