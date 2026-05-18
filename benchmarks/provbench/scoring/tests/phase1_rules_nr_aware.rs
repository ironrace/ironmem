//! SPEC §11 row 2026-05-18 (v1.2c forward path e): `compare.rs` adds a
//! `phase1_rules_nr_aware` post-hoc column to `metrics.json`. Rows whose
//! R4 trace flagged the leaf+length guard as below-floor are virtually
//! remapped from `Decision::Stale` to `Decision::NeedsRevalidation`, then
//! the §7.1 math is re-run against ground truth.

use serde_json::Value;
use std::fs;
use tempfile::TempDir;

/// Minimal `metrics.json` the baseline dir must contain so `compare::run`
/// can load it. Fields that `compare::run` does not read default to 0 /
/// empty, which is fine for a RED-phase acceptance test.
fn minimal_baseline_metrics() -> Value {
    serde_json::json!({
        "section_7_1": {
            "stale_detection":   { "precision": 0.0, "recall": 0.0, "f1": 0.0, "wilson_lower_95": 0.0 },
            "valid_retention_accuracy":          { "point": 0.0, "wilson_lower_95": 0.0 },
            "needs_revalidation_routing_accuracy": { "point": 0.0, "wilson_lower_95": 0.0 }
        },
        "section_7_2_applicable": {
            "latency_p50_ms": 7300_u64,
            "latency_p95_ms": 0_u64,
            "cost_per_correct_invalidation": { "tokens": 0.0, "usd": 0.0 }
        }
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a predictions.jsonl line using the real PredictionRow schema.
///
/// Includes an `evidence` object so that Task 6's NR-aware remap can read
/// `guard_below_floor` without schema changes to the JSONL format.
/// Today `PredictionRow` has no `evidence` field, so serde silently ignores
/// the key — this is the structural RED: the output will lack the
/// `phase1_rules_nr_aware` column entirely.
fn pred_line_with_evidence(
    i: usize,
    prediction: &str,
    ground_truth: &str,
    evidence: Value,
) -> String {
    serde_json::json!({
        "fact_id":      format!("r{i}"),
        "commit_sha":   "0000000000",
        "batch_id":     "test-batch",
        "ground_truth": ground_truth,
        "prediction":   prediction,
        "request_id":   format!("phase1:v1.2c:0000000000:{i}"),
        "wall_ms":      1_u64,
        "evidence":     evidence,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Test: NR-aware remap routes R4 guard_below_floor rows to NR
// ---------------------------------------------------------------------------

#[test]
fn nr_aware_column_reroutes_r4_guard_below_floor_rows() {
    let tmp = TempDir::new().unwrap();
    let candidate = tmp.path().join("candidate");
    let baseline = tmp.path().join("baseline");
    fs::create_dir_all(&candidate).unwrap();
    fs::create_dir_all(&baseline).unwrap();

    // Baseline dir needs a metrics.json.
    fs::write(
        baseline.join("metrics.json"),
        serde_json::to_string(&minimal_baseline_metrics()).unwrap(),
    )
    .unwrap();

    // Predictions: 4 rows.
    //   r1: phase1 says Stale via R4 with guard_below_floor=true, GT=NR
    //   r2: phase1 says Stale via R4 with guard_below_floor=false, GT=Stale
    //   r3: phase1 says Valid via R2, GT=Valid
    //   r4: phase1 says NR via R9, GT=NR
    let preds = [
        pred_line_with_evidence(
            1,
            "stale",
            "needs_revalidation",
            serde_json::json!({
                "rule": "R4",
                "reason": "stale_source_changed",
                "guard_below_floor": true
            }),
        ),
        pred_line_with_evidence(
            2,
            "stale",
            "stale",
            serde_json::json!({
                "rule": "R4",
                "reason": "stale_source_changed",
                "guard_below_floor": false
            }),
        ),
        pred_line_with_evidence(3, "valid", "valid", serde_json::json!({})),
        pred_line_with_evidence(
            4,
            "needs_revalidation",
            "needs_revalidation",
            serde_json::json!({}),
        ),
    ]
    .join("\n");

    fs::write(candidate.join("predictions.jsonl"), &preds).unwrap();
    fs::write(baseline.join("predictions.jsonl"), &preds).unwrap();

    let report = provbench_scoring::compare::run(&baseline, &candidate, "phase1_rules").unwrap();

    // The base candidate column has phase1_rules treating r1 as stale → mismatched NR.
    let phase1_nr_acc = report["phase1_rules"]["section_7_1"]
        ["needs_revalidation_routing_accuracy"]["point"]
        .as_f64()
        .unwrap();

    // The new column re-maps r1 (guard_below_floor=true) to NR, so r1 now matches GT=NR.
    let nr_aware = &report["phase1_rules_nr_aware"];
    assert!(
        nr_aware.is_object(),
        "phase1_rules_nr_aware column must appear; got: {nr_aware}"
    );
    let nr_aware_nr_acc = nr_aware["section_7_1"]["needs_revalidation_routing_accuracy"]["point"]
        .as_f64()
        .unwrap();

    assert!(
        nr_aware_nr_acc > phase1_nr_acc,
        "NR-aware remap must improve NR routing accuracy (phase1={phase1_nr_acc}, nr_aware={nr_aware_nr_acc})"
    );

    // r2 has guard_below_floor=false so it must remain Stale in the remap.
    let nr_aware_stale_recall = nr_aware["section_7_1"]["stale_detection"]["recall"]
        .as_f64()
        .unwrap();
    assert!(
        (nr_aware_stale_recall - 1.0).abs() < 1e-9,
        "r2 must remain Stale (guard_below_floor=false); recall expected 1.0, got {nr_aware_stale_recall}"
    );
}
