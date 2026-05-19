//! Side-by-side metrics builder (LLM baseline column + candidate column).
//!
//! Loads the baseline run's already-scored `metrics.json`, scores the
//! candidate run's `predictions.jsonl` against the same SPEC §7.1 axes,
//! and emits a single JSON document with both columns, per-rule confusion
//! (joined from `rule_traces.jsonl`), and SPEC §8 threshold flags.
//!
//! ## Output keys
//!
//! The top-level JSON object contains:
//! - `llm_baseline` — verbatim contents of `<baseline_run>/metrics.json`
//! - `<candidate_name>` — SPEC §7.1 metrics scored from `predictions.jsonl`
//! - `phase1_rules_nr_aware` — post-hoc NR-aware column (SPEC §11 row
//!   2026-05-18). Contains an `applicable` sentinel (`true` when at least one
//!   row was remapped) and a `rows_remapped` counter, plus a `section_7_1`
//!   sub-tree with the same shape as the candidate column.
//! - `deltas` — per-metric point deltas vs. baseline
//! - `thresholds` — SPEC §8 structured threshold-status objects
//! - `per_rule_confusion` — per-rule confusion matrix

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use crate::{metrics, PredictionRow};

/// Side-by-side metrics rollup produced by `compare::run`.
///
/// Pairs the LLM baseline's already-scored `metrics.json` against a
/// candidate (e.g. the Phase 1 rules runner) scored on the same
/// SPEC §7.1 axes, with deltas, SPEC §8 threshold-status objects, and a
/// per-rule confusion matrix joined from `rule_traces.jsonl`.
///
/// The struct is the typed counterpart to the JSON document the CLI
/// writes; consumers should prefer the JSON output for archival and
/// only deserialize back into `Compare` when programmatic access is
/// required.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Compare {
    /// LLM baseline column: contents of `<baseline_run>/metrics.json`
    /// loaded verbatim, used as the reference for deltas.
    pub llm_baseline: Value,
    /// Candidate column: SPEC §7.1 metrics scored directly from
    /// `<candidate_run>/predictions.jsonl` by `score_candidate`.
    pub candidate: Value,
    pub candidate_name: String,
    pub deltas: BTreeMap<String, f64>,
    /// SPEC §8 structured threshold-status objects keyed by gate name
    /// (`section_8_3_valid_retention_ge_0_95`, `section_8_4_latency_p50_le_727_ms`,
    /// `section_8_5_stale_recall_wlb_ge_0_30`). Each value has the shape
    /// `{status: "PASS"|"FAIL"|"SKIP", passed: bool|null, metric, observed,
    /// target, reason?}`. SPEC §11 row 2026-05-18.
    pub thresholds: serde_json::Map<String, Value>,
    /// Per-rule confusion matrix: `rule_id → "<gt>__<pred>" → count`,
    /// joined from `<candidate_run>/rule_traces.jsonl`.
    pub per_rule_confusion: BTreeMap<String, BTreeMap<String, u64>>,
}

pub fn run(baseline_run: &Path, candidate_run: &Path, candidate_name: &str) -> Result<Value> {
    // 1) Read the baseline run's pre-scored metrics.json.
    let baseline_metrics: Value = {
        let path = baseline_run.join("metrics.json");
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes)?
    };

    // 2) Score the candidate predictions.jsonl directly.
    let candidate_metrics: Value = score_candidate(candidate_run)?;

    // 3) Build deltas and SPEC §8 threshold flags.
    let stale_recall_wlb = candidate_metrics["section_7_1"]["stale_detection"]["wilson_lower_95"]
        .as_f64()
        .unwrap_or(0.0);
    let valid_acc_wlb = candidate_metrics["section_7_1"]["valid_retention_accuracy"]
        ["wilson_lower_95"]
        .as_f64()
        .unwrap_or(0.0);
    let p50 = candidate_metrics["section_7_2_applicable"]["latency_p50_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);
    let baseline_p50 = baseline_metrics["section_7_2_applicable"]["latency_p50_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);

    let mut deltas: BTreeMap<String, f64> = BTreeMap::new();
    deltas.insert(
        "stale_recall_point_delta".into(),
        metric_f64(
            &candidate_metrics,
            &["section_7_1", "stale_detection", "recall"],
        ) - metric_f64(
            &baseline_metrics,
            &["section_7_1", "stale_detection", "recall"],
        ),
    );
    deltas.insert(
        "stale_precision_point_delta".into(),
        metric_f64(
            &candidate_metrics,
            &["section_7_1", "stale_detection", "precision"],
        ) - metric_f64(
            &baseline_metrics,
            &["section_7_1", "stale_detection", "precision"],
        ),
    );
    deltas.insert(
        "valid_retention_wilson_lower_95_delta".into(),
        metric_f64(
            &candidate_metrics,
            &["section_7_1", "valid_retention_accuracy", "wilson_lower_95"],
        ) - metric_f64(
            &baseline_metrics,
            &["section_7_1", "valid_retention_accuracy", "wilson_lower_95"],
        ),
    );
    deltas.insert(
        "needs_revalidation_routing_wilson_lower_95_delta".into(),
        metric_f64(
            &candidate_metrics,
            &[
                "section_7_1",
                "needs_revalidation_routing_accuracy",
                "wilson_lower_95",
            ],
        ) - metric_f64(
            &baseline_metrics,
            &[
                "section_7_1",
                "needs_revalidation_routing_accuracy",
                "wilson_lower_95",
            ],
        ),
    );
    // NOTE: numerator and denominator are NOT in the same units — baseline is a
    // per-commit median, candidate is a per-row median. See `score_candidate`
    // for the full LATENCY METHODOLOGY block. The verbose key forces anyone
    // quoting this number to copy the disambiguation along with it.
    deltas.insert(
        "latency_p50_ratio_baseline_per_commit_to_candidate_per_row".into(),
        (baseline_p50 as f64) / (p50.max(1) as f64),
    );
    deltas.insert(
        "cost_per_correct_invalidation_usd_delta".into(),
        metric_f64(
            &candidate_metrics,
            &[
                "section_7_2_applicable",
                "cost_per_correct_invalidation",
                "usd",
            ],
        ) - metric_f64(
            &baseline_metrics,
            &[
                "section_7_2_applicable",
                "cost_per_correct_invalidation",
                "usd",
            ],
        ),
    );
    deltas.insert(
        "cost_per_correct_invalidation_tokens_delta".into(),
        metric_f64(
            &candidate_metrics,
            &[
                "section_7_2_applicable",
                "cost_per_correct_invalidation",
                "tokens",
            ],
        ) - metric_f64(
            &baseline_metrics,
            &[
                "section_7_2_applicable",
                "cost_per_correct_invalidation",
                "tokens",
            ],
        ),
    );
    // SPEC §11 row 2026-05-18 (v1.2c): thresholds are structured objects
    // with explicit PASS / FAIL / SKIP status, not bare booleans. SKIP
    // applies to §8 #5 when ground-truth stale_* count == 0 (taxonomy
    // mismatch — see SPEC §8 #5 and flask v1.2b findings).
    let gt_stale_count = count_ground_truth_stale(candidate_run)?;
    let mut thresholds = serde_json::Map::new();
    thresholds.insert(
        "section_8_3_valid_retention_ge_0_95".into(),
        threshold_status(
            valid_acc_wlb >= 0.95,
            "valid_retention_wlb",
            valid_acc_wlb,
            0.95,
        ),
    );
    thresholds.insert(
        "section_8_4_latency_p50_le_727_ms".into(),
        threshold_status(p50 <= 727, "latency_p50_ms", p50 as f64, 727.0),
    );
    thresholds.insert(
        "section_8_5_stale_recall_wlb_ge_0_30".into(),
        section_8_5_status(stale_recall_wlb, gt_stale_count),
    );

    // 4) Per-rule confusion (joined from candidate_run/rule_traces.jsonl).
    let per_rule_confusion = load_per_rule_confusion(candidate_run)?;

    // 5) NR-aware post-hoc column: R4 guard_below_floor rows remapped to NR.
    let nr_aware_metrics: Value = score_candidate_nr_aware(candidate_run)?;

    Ok(json!({
        "llm_baseline": baseline_metrics,
        candidate_name: candidate_metrics,
        "phase1_rules_nr_aware": nr_aware_metrics,
        "deltas": deltas,
        "thresholds": Value::Object(thresholds),
        "per_rule_confusion": per_rule_confusion,
    }))
}

fn score_candidate(candidate_run: &Path) -> Result<Value> {
    let preds_path = candidate_run.join("predictions.jsonl");
    let text = fs::read_to_string(&preds_path)
        .with_context(|| format!("reading {}", preds_path.display()))?;
    let mut rows: Vec<PredictionRow> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(serde_json::from_str(line)?);
    }

    let total = rows.len() as u64;
    let pop_weights: HashMap<String, f64> = HashMap::new();
    let three = metrics::three_way(&rows, &pop_weights);
    let cost = metrics::cost_per_correct_invalidation_from_totals(&rows, 0, 0.0);

    // LATENCY METHODOLOGY (read before quoting numbers).
    //
    // The `wall_ms` field name is identical for the baseline (LLM) and
    // candidate (rules) runners, but the granularity is NOT:
    //   * Baseline (LLM): per-batch wall_ms — one record per Anthropic
    //     API round-trip covering many facts. `metrics::latency()`
    //     dedupes by batch_id and sums per commit_sha to get a
    //     per-commit total, then nearest-rank p50 over commits.
    //   * Phase 1 (rules): per-row wall_ms — one record per fact's
    //     classification cost. We compute a naive floor-median over
    //     per-row values, which is the natural per-row p50.
    //
    // The `latency_p50_ms` value in the candidate column is therefore
    // a per-row median (µs-scale), while the baseline column is a
    // per-commit median (ms-to-s scale).  `latency_p50_ms_speedup`
    // in `deltas` is a useful headline but is NOT a direct apples-to-
    // apples throughput comparison — readers should treat it as
    // "baseline per-commit median ÷ candidate per-row median". The
    // SPEC §8 #4 ≤727 ms threshold is on the candidate column alone,
    // which the rules runner satisfies with margin (~2 ms).
    //
    // The right framing in the findings doc is: "Phase 1 classifies
    // a fact in median ~2 ms; the LLM baseline took median ~7.3 s
    // per commit." See benchmarks/provbench/results/phase1/
    // 2026-05-14-findings.md for the audience-facing version.
    let mut walls: Vec<u64> = rows.iter().map(|r| r.wall_ms).collect();
    walls.sort();
    let p50 = percentile_u64(&walls, 0.50);
    let p95 = percentile_u64(&walls, 0.95);

    Ok(json!({
        "row_count": total,
        "section_7_1": {
            "stale_detection": {
                "precision": three.stale_detection.precision,
                "recall": three.stale_detection.recall,
                "f1": three.stale_detection.f1,
                "wilson_lower_95": three.stale_detection.wilson_lower_95,
            },
            "valid_retention_accuracy": {
                "point": three.valid_retention_accuracy.point,
                "wilson_lower_95": three.valid_retention_accuracy.wilson_lower_95,
            },
            "needs_revalidation_routing_accuracy": {
                "point": three.needs_revalidation_routing_accuracy.point,
                "wilson_lower_95": three.needs_revalidation_routing_accuracy.wilson_lower_95,
            },
        },
        "section_7_2_applicable": {
            "latency_p50_ms": p50,
            "latency_p95_ms": p95,
            "cost_per_correct_invalidation": {
                "tokens": cost.tokens,
                "usd": cost.usd,
            },
        },
    }))
}

/// Post-hoc NR-aware column (SPEC §11 row 2026-05-18, v1.2c forward path e).
///
/// Reads `candidate_run/predictions.jsonl` and virtually remaps any row where:
///   - `evidence["rule"] == "R4"` (rule that uses the leaf+length guard)
///   - `prediction == metrics::CLASS_STALE`
///   - `evidence["guard_below_floor"] == true`
///
/// Evidence is read from the prediction row when present, with a
/// `rule_traces.jsonl` fallback for older artifacts whose predictions did
/// not yet carry evidence. The join key is `PredictionRow.row_index` when
/// present (new artifacts); falls back to the enumerate counter only when
/// `row_index` is absent AND the trace map is non-empty (legacy artifacts).
///
/// to `prediction = metrics::CLASS_NEEDS_REVAL`, then re-runs the §7.1
/// three-way math on the remapped slice. Returns a JSON value with an
/// `applicable` sentinel (`true` when at least one row was remapped), a
/// `rows_remapped` counter, and a `section_7_1` sub-tree.
fn score_candidate_nr_aware(candidate_run: &Path) -> Result<Value> {
    let preds_path = candidate_run.join("predictions.jsonl");
    let text = fs::read_to_string(&preds_path)
        .with_context(|| format!("reading {}", preds_path.display()))?;
    let trace_evidence = load_rule_trace_evidence(candidate_run)?;
    let has_traces = !trace_evidence.is_empty();
    let mut rows: Vec<PredictionRow> = Vec::new();
    let mut rows_remapped: u64 = 0;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut row: PredictionRow = serde_json::from_str(line)?;
        // Join on the typed row_index when present. Fall back to the enumerate
        // counter only for legacy artifacts (row_index absent) where the trace
        // map is non-empty — this avoids a false match on modern artifacts
        // that have no traces.
        let trace_key: i64 = match row.row_index {
            Some(ri) => ri as i64,
            None if has_traces => i as i64,
            None => -1, // no row_index and no traces — skip trace lookup
        };
        let evidence = row
            .evidence
            .as_ref()
            .or_else(|| trace_evidence.get(&trace_key));
        let guard_below_floor = evidence
            .and_then(|e| e.get("guard_below_floor"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let rule_is_r4 = evidence
            .and_then(|e| e.get("rule"))
            .and_then(|r| r.as_str())
            .map(|r| r == "R4")
            .unwrap_or(false);
        if rule_is_r4 && row.prediction == crate::metrics::CLASS_STALE && guard_below_floor {
            row.prediction = crate::metrics::CLASS_NEEDS_REVAL.into();
            rows_remapped += 1;
        }
        rows.push(row);
    }
    let three = metrics::three_way_from_rows(&rows);
    Ok(json!({
        "applicable": rows_remapped > 0,
        "rows_remapped": rows_remapped,
        "section_7_1": {
            "stale_detection": {
                "precision": three.stale_detection.precision,
                "recall": three.stale_detection.recall,
                "f1": three.stale_detection.f1,
                "wilson_lower_95": three.stale_detection.wilson_lower_95,
            },
            "valid_retention_accuracy": {
                "point": three.valid_retention_accuracy.point,
                "wilson_lower_95": three.valid_retention_accuracy.wilson_lower_95,
            },
            "needs_revalidation_routing_accuracy": {
                "point": three.needs_revalidation_routing_accuracy.point,
                "wilson_lower_95": three.needs_revalidation_routing_accuracy.wilson_lower_95,
            },
        },
    }))
}

fn load_rule_trace_evidence(candidate_run: &Path) -> Result<BTreeMap<i64, Value>> {
    let traces = candidate_run.join("rule_traces.jsonl");
    let mut out = BTreeMap::new();
    let text = match fs::read_to_string(&traces) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", traces.display())),
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)?;
        if let (Some(row_index), Some(evidence)) =
            (v["row_index"].as_i64(), v.get("evidence").cloned())
        {
            out.insert(row_index, evidence);
        }
    }
    Ok(out)
}

fn metric_f64(root: &Value, path: &[&str]) -> f64 {
    let mut current = root;
    for key in path {
        current = &current[*key];
    }
    current
        .as_f64()
        .or_else(|| current.as_u64().map(|v| v as f64))
        .unwrap_or(0.0)
}

fn percentile_u64(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Build a structured threshold-status object for SPEC §8 thresholds
/// whose verdict is a simple pass/fail (no SKIP semantics).
fn threshold_status(passed: bool, metric_name: &str, observed: f64, target: f64) -> Value {
    json!({
        "status": if passed { "PASS" } else { "FAIL" },
        "passed": passed,
        "metric": metric_name,
        "observed": observed,
        "target": target,
    })
}

/// SPEC §8 #5 SKIP-aware status. See SPEC §11 row 2026-05-18.
fn section_8_5_status(stale_recall_wlb: f64, gt_stale_count: u64) -> Value {
    if gt_stale_count == 0 {
        return json!({
            "status": "SKIP",
            "passed": Value::Null,
            "metric": "stale_recall_wlb",
            "observed": Value::Null,
            "target": 0.30,
            "reason": "ground_truth_stale_count_is_zero",
        });
    }
    let passed = stale_recall_wlb >= 0.30;
    json!({
        "status": if passed { "PASS" } else { "FAIL" },
        "passed": passed,
        "metric": "stale_recall_wlb",
        "observed": stale_recall_wlb,
        "target": 0.30,
    })
}

/// Count rows in `predictions.jsonl` whose coalesced `ground_truth` equals
/// [`metrics::CLASS_STALE`]. Handles both lowercase labeler tags (`stale`,
/// `stale_source_changed`, …) and PascalCase labeler tags (`StaleSourceChanged`,
/// `StaleSourceDeleted`, `StaleSymbolRenamed`) via [`metrics::coalesce`].
/// Returns 0 if the file is empty.
fn count_ground_truth_stale(candidate_run: &Path) -> Result<u64> {
    let preds_path = candidate_run.join("predictions.jsonl");
    let text = fs::read_to_string(&preds_path)
        .with_context(|| format!("reading {}", preds_path.display()))?;
    let mut count = 0u64;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line)?;
        if let Some(gt) = row.get("ground_truth").and_then(|g| g.as_str()) {
            if metrics::coalesce(gt) == metrics::CLASS_STALE {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn load_per_rule_confusion(
    candidate_run: &Path,
) -> Result<BTreeMap<String, BTreeMap<String, u64>>> {
    let traces = candidate_run.join("rule_traces.jsonl");
    let preds = candidate_run.join("predictions.jsonl");
    let mut row_to_rule: BTreeMap<i64, String> = BTreeMap::new();
    {
        let text = match fs::read_to_string(&traces) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", traces.display())),
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line)?;
            let row_index = v["row_index"].as_i64().unwrap_or(-1);
            let rule_id = v["rule_id"].as_str().unwrap_or("?").to_string();
            row_to_rule.insert(row_index, rule_id);
        }
    }
    let mut out: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let text = fs::read_to_string(&preds)?;
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let r: PredictionRow = serde_json::from_str(line)?;
        let rule = row_to_rule
            .get(&(i as i64))
            .cloned()
            .unwrap_or_else(|| "?".to_string());
        let bucket = out.entry(rule).or_default();
        let key = format!(
            "{}__{}",
            r.ground_truth.to_lowercase(),
            r.prediction.to_lowercase()
        );
        *bucket.entry(key).or_insert(0) += 1;
    }
    Ok(out)
}
