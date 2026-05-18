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
    /// Optional evidence blob emitted by rule-based runners.
    /// Phase 1 R4 sets `{"rule": "R4", "guard_below_floor": <bool>, ...}`.
    /// Absent on baseline rows and on legacy artifacts — `#[serde(default)]`
    /// keeps those rows deserializable, and `skip_serializing_if` keeps legacy
    /// fixtures byte-identical when they round-trip through `PredictionRow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
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
