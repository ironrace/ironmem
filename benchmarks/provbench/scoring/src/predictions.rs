use serde::{Deserialize, Serialize};

/// Per-row checkpoint persisted to `predictions.jsonl`.
///
/// One row per line. JSON field order is fixed by serde derive order;
/// existing rows are never rewritten so determinism is preserved across
/// resumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionRow {
    pub fact_id: String,
    pub commit_sha: String,
    pub batch_id: String,
    pub ground_truth: String,
    pub prediction: String,
    /// Runner-specific identifier:
    ///   - Baseline emits the Anthropic API request id (`req_…`).
    ///   - Phase 1 emits `phase1:<rule_set_version>:<commit_sha>:<row_index>`.
    ///
    /// Format is opaque to the scorer; it is preserved verbatim in
    /// `predictions.jsonl` for audit / debugging.
    pub request_id: String,
    pub wall_ms: u64,
    /// Microsecond-resolution rule-chain latency for a single row.
    ///
    /// Added in v1.2c+ to give meaningful latency reporting on sub-millisecond
    /// Python rule-chain work (flask predictions had `wall_ms: 0` across the
    /// entire subset because the structural rule chain runs in 100–900 μs per
    /// row, which rounds to 0 at millisecond granularity).
    ///
    /// `wall_ms` retains its SPEC §8 #4 contract (integer milliseconds, used
    /// for the ≤ 727 ms latency threshold). `wall_us` is purely additive
    /// precision.
    ///
    /// `None` on legacy artifacts (which pre-date this field) and on baseline
    /// LLM-runner output (where μs precision has no useful interpretation for
    /// per-batch API round-trips).
    ///
    /// `#[serde(default)]` + `skip_serializing_if` keeps legacy v1.2c
    /// artifacts byte-stable through round-trip: absent key → `None` on
    /// deserialize; `None` → no key on serialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_us: Option<u64>,
    /// Optional evidence blob emitted by rule-based runners.
    /// Phase 1 R4 sets `{"rule": "R4", "guard_below_floor": <bool>, ...}`.
    /// Absent on baseline rows and on legacy artifacts — `#[serde(default)]`
    /// keeps those rows deserializable, and `skip_serializing_if` keeps legacy
    /// fixtures byte-identical when they round-trip through `PredictionRow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    /// SQLite `row_index` counter from the runner (0-based, matches
    /// `rule_traces.jsonl` `row_index` field). Absent on baseline rows and
    /// legacy artifacts; `score_candidate_nr_aware` joins on this when
    /// present and falls back to the enumerate counter only on legacy
    /// artifacts where the field is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_index: Option<u64>,
}

/// Read-side mirror of `run_meta.json` — only the fields the scorer
/// consumes. Optional fields default so partial/legacy run-metas still
/// deserialise.
#[derive(Debug, Clone, Deserialize)]
pub struct RunResult {
    #[serde(default)]
    pub total_cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
}
